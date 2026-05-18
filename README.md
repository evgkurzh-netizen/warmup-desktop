# Warmup Desktop — release repository

This public repository exists **only to produce signed installers** of the
Warmup operator desktop app via free GitHub Actions. It contains:

- `tools/desktop/` — Tauri 2 Rust shell + Playwright capture sidecar
- `.github/workflows/desktop-release.yml` — multi-OS build matrix

The actual product (dashboard, backend) lives in a separate private
codebase and is **not** included here.

## Releasing a new version

1. Bump `version` in `tools/desktop/src-tauri/tauri.conf.json` and
   `tools/desktop/src-tauri/Cargo.toml`.
2. Commit, push, then:

   ```bash
   git tag desktop-v0.1.1
   git push origin desktop-v0.1.1
   ```

3. Wait ~15 min. Installers appear under **Releases**.

## Required repo secrets

| Secret                                  | What                                                      |
| --------------------------------------- | --------------------------------------------------------- |
| `TAURI_UPDATER_PUBKEY`                  | Public half of the updater signing keypair                 |
| `TAURI_SIGNING_PRIVATE_KEY`             | Private half (paste raw file contents)                     |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`    | Optional — only if you protected the private key           |

Generate the pair once on your dev machine:

```bash
pnpm --filter @workspace/desktop exec tauri signer generate -w tauri-updater.key
```

`tauri-updater.key.pub` → `TAURI_UPDATER_PUBKEY`,
`tauri-updater.key`    → `TAURI_SIGNING_PRIVATE_KEY`.

## Updater feed

The desktop app polls
`https://warm-acc.fvds.ru/api/updates/latest.json`. On the VPS set:

```
DESKTOP_RELEASES_REPO=<owner>/<this-repo>
```

so the API can discover this repo's releases.
