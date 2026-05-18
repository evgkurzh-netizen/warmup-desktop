# Warmup Desktop (Tauri)

Operator console for the Warmup service. Wraps the live dashboard at
`https://warm-acc.fvds.ru/` in a native window and ships the cookie-capture
flow as a sidecar binary, so the operator no longer needs Node, npx, or a
terminal.

## What it does

- **Live dashboard.** A native window points at `warm-acc.fvds.ru`. Every
  UI change you ship to the server lands in the app on next reload — no
  reinstall.
- **OS-keychain token storage.** On first launch a small native window
  collects your owner API token and saves it to the system keychain
  (macOS Keychain / Windows Credential Manager). The token is **never**
  written to localStorage, cookies, or any file. A `fetch` wrapper
  installed before any page script attaches `x-api-key: <token>` to all
  same-origin `/api/` requests.
- **One-click cookie capture.** The Accounts page gets a "Capture cookies
  (desktop)" button. Click → the bundled sidecar opens Chromium with the
  account's proxy + fingerprint → you log into Google by hand → Chromium
  closes → cookies are POSTed back to the API. Live progress (started /
  progress / done / error) is forwarded into the dashboard via toasts and
  refreshes the cookies-meta query when done.
- **Chromium auto-download.** The welcome window installs Playwright's
  Chromium (~150 MB) on first launch into the standard
  `ms-playwright` cache. No manual `npx playwright install`.
- **Signed auto-update.** The app polls
  `https://warm-acc.fvds.ru/api/updates/latest.json` on launch, downloads
  the new bundle in the background and prompts the operator to restart.

## Code signing

We deliberately don't sign. macOS users right-click → Open the first
time; Windows users hit "More info → Run anyway" on the SmartScreen warning.

## Layout

```
tools/desktop/
├── shell/
│   ├── index.html       # placeholder for the main window before it loads warm-acc.fvds.ru
│   └── welcome.html     # local first-launch UI (token + chromium download)
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   ├── capabilities/
│   │   ├── welcome.json # invoke + dialog perms scoped to the welcome window
│   │   └── main.json    # shell:open + updater only; remote origin locked
│   └── src/
│       ├── main.rs
│       └── lib.rs       # window flow, keychain, sidecar spawn, capture-event eval
└── sidecar/
    ├── package.json     # pkg config bundling Playwright's registry
    ├── build.mjs        # esbuild → @yao-pkg/pkg per-platform target
    └── src/capture.ts   # --install-chromium and capture modes
```

## Release pipeline

The repo lives on **public GitHub** so Actions minutes are free for OSS.
A push of a tag matching `desktop-v*` triggers
`.github/workflows/desktop-release.yml` to build:

| Runner          | rustTarget                  | Artefact        |
| --------------- | --------------------------- | --------------- |
| `macos-14`      | `aarch64-apple-darwin`      | `.dmg`, `.sig`  |
| `macos-13`      | `x86_64-apple-darwin`       | `.dmg`, `.sig`  |
| `windows-latest`| `x86_64-pc-windows-msvc`    | `.exe`, `.sig`  |

A final job collates everything into a GitHub Release.

### One-time setup

1. **Generate the updater keypair** on your laptop:

   ```bash
   pnpm --filter @workspace/desktop exec tauri signer generate
   ```

   Save the **public key** as the `TAURI_UPDATER_PUBKEY` GitHub
   secret (CI substitutes it into `tauri.conf.json` before building) and
   the **private key** as `TAURI_SIGNING_PRIVATE_KEY` (+ an optional
   `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if you protected it).

2. **Set the updater feed env var** on the VPS so the API endpoint can
   discover releases:

   ```
   DESKTOP_RELEASES_REPO=<owner>/<repo>
   ```

   Without this, `/api/updates/latest.json` returns `204` and the app
   simply skips the update check.

3. **Push the first tag:**

   ```bash
   git tag desktop-v0.1.0
   git push origin desktop-v0.1.0
   ```

## Local development

You need Rust + Cargo and Node 20. Replit's sandbox doesn't have Rust, so
all real testing happens locally or in CI.

```bash
# build the sidecar binary for your host platform
cd tools/desktop/sidecar
pnpm install
node ./build.mjs --target=auto

# run the shell
cd ..
pnpm tauri dev
```

The desktop shell loads `https://warm-acc.fvds.ru/`. Out-of-band
navigations open in your default browser; the only origin trusted with
the injected owner token is the dashboard itself.
