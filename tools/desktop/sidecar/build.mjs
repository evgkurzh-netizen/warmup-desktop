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
// Node 22 is required on macOS 15 / Apple Silicon: the V8 in Node 20 fails
// at startup with "Fatal process OOM in Failed to reserve virtual memory for
// CodeRange" because of macOS 15's new VM layout. The fix landed in V8 12.x
// (shipped in Node 22). Windows/Linux work fine with either; keep them on
// 22 for parity.
const TARGETS = {
  "darwin-arm64": { pkg: "node22-macos-arm64", triple: "aarch64-apple-darwin" },
  "darwin-x64": { pkg: "node22-macos-x64", triple: "x86_64-apple-darwin" },
  "win-x64": { pkg: "node22-win-x64", triple: "x86_64-pc-windows-msvc" },
  "linux-x64": { pkg: "node22-linux-x64", triple: "x86_64-unknown-linux-gnu" },
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
// shell:true is required on Windows where there's no bare `npx` —
// only `npx.cmd`. execFileSync without a shell can't resolve that.
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
  { stdio: "inherit", cwd: repoSidecar, shell: true },
);

// pkg works by appending the bundled script + V8 snapshot to the end of
// the base Node binary. On macOS that invalidates the signature pkg-fetch
// ships with, and the macOS 15 / Apple Silicon loader SIGTRAPs the binary
// before any user code runs. Re-sign ad-hoc to satisfy the loader, AND
// attach the JIT entitlements V8 needs to allocate MAP_JIT pages -- the
// upstream pkg-fetched Node ships with these entitlements; we have to
// re-attach them since `codesign --force` strips them.
if (targetKey === "darwin-arm64" || targetKey === "darwin-x64") {
  const entitlements = resolve(repoSidecar, "entitlements.plist");
  console.log(`[sidecar] re-signing ${outPath} with JIT entitlements …`);
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
      outPath,
    ],
    { stdio: "inherit" },
  );
}

const ext = targetKey === "win-x64" ? ".exe" : "";
const finalPath = resolve(tauriBinaries, `capture-${target.triple}${ext}`);
copyFileSync(outPath, finalPath);
console.log(`[sidecar] staged at ${finalPath}`);
