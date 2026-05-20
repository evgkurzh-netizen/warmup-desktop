/**
 * Capture sidecar for the Warmup desktop app.
 *
 * Modes:
 *   --install-chromium           Bootstraps Playwright's Chromium into the
 *                                user's standard cache dir. Used by the
 *                                first-launch welcome flow. Emits human-
 *                                readable progress lines to stdout.
 *   --base-url … --token … --account … [--emit-json]
 *                                Capture mode. Same protocol as
 *                                tools/capture-tool. With --emit-json each
 *                                stdout line is a single JSON event the
 *                                Tauri shell forwards to the dashboard.
 */
import { chromium, type BrowserContext, type Page } from "playwright";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

interface ProxyConfig {
  type: "http" | "socks5";
  host: string;
  port: number;
  username?: string | null;
  password?: string | null;
}

interface FingerprintProfile {
  device: "desktop" | "mobile";
  userAgent: string;
  languages: string[];
  timezone: string;
  screenWidth: number;
  screenHeight: number;
  deviceScaleFactor: number;
  platform: string;
  hardwareConcurrency: number;
  deviceMemory: number;
  webglVendor: string;
  webglRenderer: string;
  canvasNoiseSeed: number;
  audioNoiseSeed: number;
}

interface LoginContext {
  accountId: string;
  label: string;
  hasCookies: boolean;
  fingerprint: FingerprintProfile;
  proxy: ProxyConfig | null;
}

interface Args {
  baseUrl: string;
  token: string;
  accountId: string;
  startUrl: string;
  timeoutMin: number;
  emitJson: boolean;
}

function parseArgs(argv: string[]): Args {
  const args: Record<string, string> = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a) continue;
    if (a.startsWith("--")) {
      const k = a.slice(2);
      const v = argv[i + 1];
      if (!v || v.startsWith("--")) {
        args[k] = "true";
      } else {
        args[k] = v;
        i++;
      }
    }
  }
  const baseUrl = args["base-url"] ?? process.env["WARMUP_BASE_URL"] ?? "";
  const token = args["token"] ?? process.env["OWNER_TOKEN"] ?? "";
  const accountId = args["account"] ?? "";
  if (!baseUrl) throw new Error("Missing --base-url");
  if (!token) throw new Error("Missing --token");
  if (!accountId) throw new Error("Missing --account");
  return {
    baseUrl: baseUrl.replace(/\/$/, ""),
    token,
    accountId,
    startUrl: args["url"] ?? "https://accounts.google.com/",
    timeoutMin: Number(args["timeout-min"] ?? 15),
    emitJson: args["emit-json"] === "true",
  };
}

let EMIT_JSON = false;
function emit(ev: Record<string, unknown>): void {
  if (EMIT_JSON) {
    process.stdout.write(JSON.stringify(ev) + "\n");
  } else {
    process.stdout.write(
      `[capture] ${ev.type}${ev.message ? `: ${ev.message}` : ""}\n`,
    );
  }
}

async function fetchLoginContext(args: Args): Promise<LoginContext> {
  const res = await fetch(
    `${args.baseUrl}/api/accounts/${args.accountId}/login-context`,
    { headers: { "x-api-key": args.token } },
  );
  if (!res.ok) {
    throw new Error(`GET /login-context → ${res.status}: ${await res.text()}`);
  }
  return (await res.json()) as LoginContext;
}

async function uploadCookies(
  args: Args,
  storageState: Record<string, unknown>,
): Promise<void> {
  const res = await fetch(
    `${args.baseUrl}/api/accounts/${args.accountId}/cookies`,
    {
      method: "PUT",
      headers: { "x-api-key": args.token, "content-type": "application/json" },
      body: JSON.stringify({ storageState }),
    },
  );
  if (!res.ok) {
    throw new Error(`PUT /cookies → ${res.status}: ${await res.text()}`);
  }
}

const GOOGLE_LOGIN_COOKIES = [
  "__Secure-3PSID",
  "SID",
  "SAPISID",
  "__Secure-1PSID",
];

/**
 * Locate a real Chromium-based browser the user already has installed.
 * Playwright's `channel: "chrome"` only works when Chrome is at the exact
 * standard system path AND has been registered via `playwright install
 * chrome`. We bypass that by probing the disk ourselves and passing
 * `executablePath` directly — this is far more reliable across user
 * machines, including ones where Chrome was installed under
 * ~/Applications, Brave is installed instead of Chrome, or only Edge is
 * present.
 *
 * Returns `null` when no real browser is found; the caller falls back to
 * the bundled Playwright Chromium (which Google's sign-in flow blocks,
 * but we keep it for environments without a real browser, e.g. CI).
 */
function findRealBrowserPath(): { path: string; name: string } | null {
  const candidates: Array<{ path: string; name: string }> = [];
  if (process.platform === "darwin") {
    const home = os.homedir();
    for (const root of ["/Applications", path.join(home, "Applications")]) {
      candidates.push(
        { path: path.join(root, "Google Chrome.app/Contents/MacOS/Google Chrome"), name: "Google Chrome" },
        { path: path.join(root, "Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"), name: "Google Chrome Beta" },
        { path: path.join(root, "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"), name: "Google Chrome Canary" },
        { path: path.join(root, "Microsoft Edge.app/Contents/MacOS/Microsoft Edge"), name: "Microsoft Edge" },
        { path: path.join(root, "Brave Browser.app/Contents/MacOS/Brave Browser"), name: "Brave Browser" },
        { path: path.join(root, "Arc.app/Contents/MacOS/Arc"), name: "Arc" },
      );
    }
  } else if (process.platform === "win32") {
    const pf = process.env["ProgramFiles"] ?? "C:\\Program Files";
    const pfx86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    const lad = process.env["LOCALAPPDATA"] ?? path.join(os.homedir(), "AppData", "Local");
    candidates.push(
      { path: path.join(pf, "Google", "Chrome", "Application", "chrome.exe"), name: "Google Chrome" },
      { path: path.join(pfx86, "Google", "Chrome", "Application", "chrome.exe"), name: "Google Chrome" },
      { path: path.join(lad, "Google", "Chrome", "Application", "chrome.exe"), name: "Google Chrome" },
      { path: path.join(pf, "Microsoft", "Edge", "Application", "msedge.exe"), name: "Microsoft Edge" },
      { path: path.join(pfx86, "Microsoft", "Edge", "Application", "msedge.exe"), name: "Microsoft Edge" },
      { path: path.join(lad, "BraveSoftware", "Brave-Browser", "Application", "brave.exe"), name: "Brave Browser" },
    );
  } else {
    candidates.push(
      { path: "/usr/bin/google-chrome", name: "Google Chrome" },
      { path: "/usr/bin/google-chrome-stable", name: "Google Chrome" },
      { path: "/usr/bin/microsoft-edge", name: "Microsoft Edge" },
      { path: "/usr/bin/brave-browser", name: "Brave Browser" },
    );
  }
  for (const c of candidates) {
    try {
      if (fs.existsSync(c.path)) return c;
    } catch {
      /* ignore */
    }
  }
  return null;
}

function profileDir(accountId: string): string {
  const base =
    process.platform === "darwin"
      ? path.join(os.homedir(), "Library", "Application Support", "Warmup", "profiles")
      : process.platform === "win32"
      ? path.join(process.env["LOCALAPPDATA"] ?? path.join(os.homedir(), "AppData", "Local"), "Warmup", "profiles")
      : path.join(os.homedir(), ".config", "warmup", "profiles");
  const dir = path.join(base, accountId);
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Brave/Chrome's persistent session manager will re-open every tab that was
 * open at last shutdown. Because we forcibly kill the browser on SIGTERM,
 * every aborted capture session leaves "Current Session" / "Current Tabs"
 * snapshots behind, so the NEXT launch reopens that tab — and the one
 * before that — turning a single click into a cascade of accounts.google
 * tabs that keep stealing focus. Wiping these state files before launch
 * keeps each capture session starting from a single empty tab while still
 * preserving the Cookies / Local Storage DB we actually care about.
 */
function clearSessionRestoreState(userDataDir: string): void {
  // We wipe SESSION + UI PREFERENCE state but keep the Cookies / Local
  // Storage / IndexedDB DBs that actually represent the user's Google
  // login. "Preferences" / "Local State" carry the UI locale, the
  // restore-on-startup setting, the list of pinned tabs etc. — leaving
  // them in place was causing two visible bugs: (a) Brave kept rendering
  // its UI in Korean because the first run wrote intl.accept_languages
  // from the fingerprint, and (b) Brave kept reopening the prior auth
  // tab on every launch despite us wiping Current Session, because the
  // "open tabs from last session" flag in Preferences was sticky.
  const targets = [
    path.join(userDataDir, "Default", "Current Session"),
    path.join(userDataDir, "Default", "Current Tabs"),
    path.join(userDataDir, "Default", "Last Session"),
    path.join(userDataDir, "Default", "Last Tabs"),
    path.join(userDataDir, "Default", "Sessions"),
    path.join(userDataDir, "Default", "Preferences"),
    path.join(userDataDir, "Default", "Secure Preferences"),
    path.join(userDataDir, "Local State"),
  ];
  for (const target of targets) {
    try {
      fs.rmSync(target, { recursive: true, force: true });
    } catch {
      /* ignore */
    }
  }
}

/**
 * Best-effort reachability check for the account's proxy. Issued before we
 * spawn the browser so that an obviously dead proxy fails in <5s with a
 * clear message instead of leaving the user staring at a blank Brave window
 * while Playwright's page.goto burns the full 120s navigation timeout. We
 * deliberately only test that the proxy's TCP port is accepting
 * connections — we cannot easily verify that the proxy can actually reach
 * google.com (especially for SOCKS5 + auth) without pulling in another
 * dependency, and a TCP probe catches the overwhelmingly common case of a
 * dead/unreachable proxy host.
 */
async function probeProxyReachable(
  proxy: ProxyConfig,
): Promise<{ ok: true } | { ok: false; reason: string }> {
  return new Promise((resolve) => {
    const sock = net.createConnection({ host: proxy.host, port: proxy.port });
    const timer = setTimeout(() => {
      sock.destroy();
      resolve({
        ok: false,
        reason: `TCP connect to ${proxy.host}:${proxy.port} timed out after 5s`,
      });
    }, 5000);
    sock.once("connect", () => {
      clearTimeout(timer);
      sock.destroy();
      resolve({ ok: true });
    });
    sock.once("error", (err) => {
      clearTimeout(timer);
      resolve({
        ok: false,
        reason: `TCP connect to ${proxy.host}:${proxy.port} failed: ${err.message}`,
      });
    });
  });
}

async function launch(
  ctx: LoginContext,
): Promise<{ context: BrowserContext; page: Page }> {
  const userDataDir = profileDir(ctx.accountId);
  clearSessionRestoreState(userDataDir);
  // Choose the UI locale that the browser CHROME is rendered in. This is
  // separate from the navigator.language emulation done by stealthScript and
  // by the `locale` option below. We force a Latin-script UI so the address
  // bar / menus are never in CJK characters the user can't read, even if the
  // fingerprint's primary language is e.g. zh-CN.
  const fpLang = (ctx.fingerprint.languages[0] ?? "en-US").toLowerCase();
  const uiLang = /^(en|ru|de|fr|es|pt|it|nl|pl|tr|uk)\b/.test(fpLang)
    ? fpLang
    : "en-US";
  const launchOptions = {
    headless: false,
    // Drop Playwright's default `--enable-automation` flag. It is THE single
    // strongest tell for Google's "this browser or app may not be secure"
    // gate: it sets `navigator.webdriver = true`, surfaces the "Chrome is
    // being controlled by automated test software" infobar, and disables a
    // few Chrome features (e.g. password manager) Google's bot detection
    // looks for. With this removed Brave/Chrome looks like a normal
    // user-launched session.
    ignoreDefaultArgs: ["--enable-automation"],
    args: [
      // NOTE: --no-sandbox / --disable-setuid-sandbox are Linux-only
      // sandboxing controls. Passing them on macOS/Windows is harmless to
      // Chromium itself, but Brave surfaces a red "you are using an
      // unsupported command-line flag, stability and security will suffer"
      // banner over the page which spooked the user. The desktop app only
      // targets macOS and Windows, so we omit them entirely.
      "--no-first-run",
      "--no-default-browser-check",
      "--no-service-autorun",
      // Disable Brave's own onboarding tabs (rewards, wallet, web-discovery),
      // session-restore prompts, AND Brave Shields. Shields blocks a long
      // list of Google trackers by default which can stall accounts.google.com
      // sub-resource loads and leave the sign-in page spinning.
      "--disable-features=Translate,InfiniteSessionRestore,BraveRewards,BraveAds,BraveWayback,BraveSearchOmnibox,BraveWelcomeUI,WebOTP,BraveShields,BraveAdblockExperimentalListDefault",
      "--disable-brave-update",
      "--disable-component-update",
      "--restore-last-session=false",
      // Always force an English browser UI regardless of the fingerprint
      // language. navigator.language is still emulated separately in
      // stealthScript so the SITE sees the fingerprint language.
      "--lang=en-US",
    ],
    userAgent: ctx.fingerprint.userAgent,
    // NOTE: we intentionally do NOT pass `locale` here. Playwright writes
    // the locale option into Chromium's --lang arg AND into the profile's
    // Preferences file (intl.app_locale), which is what kept rendering
    // Brave's UI in Korean/Chinese for users whose fingerprint had a CJK
    // primary language. Browser UI language is now hard-coded to en-US via
    // the --lang arg above; navigator.language and Accept-Language are
    // emulated separately (stealthScript + extraHTTPHeaders below).
    timezoneId: ctx.fingerprint.timezone,
    viewport: {
      width: ctx.fingerprint.screenWidth,
      height: ctx.fingerprint.screenHeight,
    },
    deviceScaleFactor: ctx.fingerprint.deviceScaleFactor,
    isMobile: ctx.fingerprint.device === "mobile",
    hasTouch: ctx.fingerprint.device === "mobile",
    extraHTTPHeaders: { "Accept-Language": ctx.fingerprint.languages.join(",") },
    proxy: ctx.proxy
      ? {
          server: `${ctx.proxy.type === "socks5" ? "socks5" : "http"}://${ctx.proxy.host}:${ctx.proxy.port}`,
          username: ctx.proxy.username ?? undefined,
          password: ctx.proxy.password ?? undefined,
        }
      : undefined,
  };

  // Try the user's REAL browser first. Google's sign-in flow blocks
  // Playwright's vanilla Chromium with "This browser or app may not be
  // secure" because it ships without Widevine and Google's baked-in API
  // keys. Probing the disk ourselves works around Playwright's `channel:
  // "chrome"` requiring `playwright install chrome` to register the path,
  // and lets us also accept Edge, Brave, Arc, etc.
  let context: BrowserContext;
  const realBrowser = findRealBrowserPath();
  if (realBrowser) {
    try {
      context = await chromium.launchPersistentContext(userDataDir, {
        ...launchOptions,
        executablePath: realBrowser.path,
      });
      emit({ type: "progress", message: `using ${realBrowser.name} at ${realBrowser.path}` });
    } catch (err) {
      emit({
        type: "progress",
        message: `${realBrowser.name} launch failed (${err instanceof Error ? err.message : String(err)}); using bundled chromium — Google may block`,
      });
      context = await chromium.launchPersistentContext(userDataDir, launchOptions);
    }
  } else {
    emit({
      type: "progress",
      message: "No Chrome/Edge/Brave installed; using bundled Chromium. Google's sign-in will likely block this browser — install Google Chrome and retry.",
    });
    context = await chromium.launchPersistentContext(userDataDir, launchOptions);
  }
  await context.addInitScript(stealthScript(ctx.fingerprint));
  // Per-page diagnostics. The user reported the proxy works externally but
  // page.goto still times out — we need to see what Chromium's network
  // stack is actually doing. requestfailed surfaces proxy errors like
  // ERR_TUNNEL_CONNECTION_FAILED, ERR_PROXY_CONNECTION_FAILED,
  // ERR_PROXY_AUTH_REQUESTED, etc.; console catches Chromium-side warnings;
  // page/framenavigated still help debug the tab cascade we fixed in v0.1.29.
  const attachDiagnostics = (p: Page) => {
    emit({ type: "progress", message: `new page opened: ${p.url() || "(blank)"}` });
    p.on("framenavigated", (frame) => {
      if (frame === p.mainFrame()) {
        emit({ type: "progress", message: `page navigated: ${frame.url()}` });
      }
    });
    p.on("requestfailed", (req) => {
      const failure = req.failure();
      emit({
        type: "progress",
        message: `request failed: ${req.method()} ${req.url()} — ${failure?.errorText ?? "unknown"}`,
      });
    });
    p.on("response", (resp) => {
      const status = resp.status();
      if (status >= 400) {
        emit({
          type: "progress",
          message: `response ${status} ${resp.url()}`,
        });
      }
    });
    p.on("console", (msg) => {
      const t = msg.type();
      if (t === "error" || t === "warning") {
        emit({ type: "progress", message: `console.${t}: ${msg.text().slice(0, 300)}` });
      }
    });
    p.on("close", () => {
      emit({ type: "progress", message: `page closed: ${p.url() || "(blank)"}` });
    });
  };
  context.on("page", attachDiagnostics);
  const page = context.pages()[0] ?? (await context.newPage());
  // The very first page already exists by the time we attach the listener,
  // so we have to wire diagnostics on it manually — otherwise we'd miss
  // every event on the only page we actually navigate.
  attachDiagnostics(page);
  return { context, page };
}

async function waitForLogin(
  exportState: () => Promise<Record<string, unknown>>,
  timeoutMs: number,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const state = await exportState();
    const cookies =
      (state["cookies"] as Array<{ name?: string; domain?: string }> | undefined) ??
      [];
    const signedIn = cookies.some(
      (c) =>
        GOOGLE_LOGIN_COOKIES.includes(c.name ?? "") &&
        (c.domain ?? "").includes("google"),
    );
    if (signedIn) return state;
    await new Promise((r) => setTimeout(r, 2_000));
  }
  throw new Error(
    `Timed out after ${Math.round(timeoutMs / 60_000)} min waiting for Google login.`,
  );
}

async function installChromium(): Promise<void> {
  // Use Playwright's own internal registry to download Chromium into its
  // standard cache directory. This is the same code path `npx playwright
  // install chromium` triggers.
  process.stdout.write("Resolving Playwright registry…\n");
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const reg: any = await import("playwright-core/lib/server/registry");
  const registry = reg.registry as {
    executables(): Array<{ name: string }>;
    install(
      executables: Array<{ name: string }>,
      forceReinstall: boolean,
    ): Promise<void>;
  };
  const wanted = registry
    .executables()
    .filter((e) => e.name === "chromium" || e.name === "chromium-headless-shell");
  if (wanted.length === 0) {
    throw new Error("Playwright has no chromium executable registered.");
  }
  process.stdout.write(
    `Installing ${wanted.map((w) => w.name).join(", ")} …\n`,
  );
  await registry.install(wanted, false);
  process.stdout.write("Chromium ready.\n");
}

async function main(): Promise<void> {
  const raw = process.argv.slice(2);
  if (raw.includes("--install-chromium")) {
    await installChromium();
    return;
  }
  const args = parseArgs(raw);
  EMIT_JSON = args.emitJson;

  emit({ type: "progress", message: `Fetching login context for ${args.accountId}` });
  const ctx = await fetchLoginContext(args);
  emit({
    type: "started",
    account_id: ctx.accountId,
    label: ctx.label,
    device: ctx.fingerprint.device,
    has_proxy: !!ctx.proxy,
  });

  if (ctx.proxy) {
    emit({
      type: "progress",
      message: `probing proxy ${ctx.proxy.host}:${ctx.proxy.port} (${ctx.proxy.type})`,
    });
    const probe = await probeProxyReachable(ctx.proxy);
    if (!probe.ok) {
      emit({
        type: "error",
        message: `Proxy unreachable: ${probe.reason}. Check the proxy in the dashboard — the account's proxy server is down or unreachable from this machine.`,
      });
      process.exit(1);
    }
    emit({ type: "progress", message: "proxy reachable" });
    const scheme = ctx.proxy.type === "socks5" ? "socks5" : "http";
    const hasAuth = !!(ctx.proxy.username || ctx.proxy.password);
    emit({
      type: "progress",
      message: `passing ${scheme}://${ctx.proxy.host}:${ctx.proxy.port} to Chromium (auth=${hasAuth ? "yes" : "no"})`,
    });
  }

  const { context, page } = await launch(ctx);
  // Make absolutely sure the browser process dies when this sidecar is
  // killed (Tauri sends SIGTERM on app exit; before this handler the browser
  // window stayed alive after the desktop app was closed, and clicking
  // Capture again spawned yet another orphan browser).
  let closing = false;
  const shutdown = async (signal: NodeJS.Signals) => {
    if (closing) return;
    closing = true;
    process.stderr.write(`[capture] received ${signal}, closing browser\n`);
    try {
      await context.close();
    } catch {
      /* ignore */
    }
    process.exit(0);
  };
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.on("SIGINT", () => void shutdown("SIGINT"));
  process.on("SIGHUP", () => void shutdown("SIGHUP"));
  // If the user closes the browser window directly, treat that as a cancel
  // and exit cleanly instead of hanging in waitForLogin.
  context.on("close", () => {
    if (!closing) {
      closing = true;
      emit({ type: "error", message: "Browser window was closed before login completed." });
      process.exit(1);
    }
  });

  try {
    // waitUntil "domcontentloaded" gives Chromium time to actually parse the
    // sign-in page; "commit" only waits for response headers and surfaced as
    // an unhelpful 90s timeout when the proxy was slow. Bumped to 120s
    // because residential proxies routinely take 20-40s on the first hop.
    await page.goto(args.startUrl, { waitUntil: "domcontentloaded", timeout: 120_000 });
    emit({
      type: "progress",
      message: `Waiting up to ${args.timeoutMin} min for Google login cookies.`,
    });
    const state = await waitForLogin(
      async () => (await context.storageState()) as unknown as Record<string, unknown>,
      args.timeoutMin * 60_000,
    );
    const cookieCount = ((state["cookies"] as unknown[] | undefined) ?? []).length;
    await uploadCookies(args, state);
    emit({
      type: "done",
      account_id: ctx.accountId,
      cookie_count: cookieCount,
    });
  } finally {
    closing = true;
    try {
      await context.close();
    } catch {
      /* ignore */
    }
  }
}

main().catch((err) => {
  emit({
    type: "error",
    message: err instanceof Error ? err.message : String(err),
  });
  process.exit(1);
});

function stealthScript(fp: FingerprintProfile): string {
  const data = JSON.stringify({
    languages: fp.languages,
    platform: fp.platform,
    hardwareConcurrency: fp.hardwareConcurrency,
    deviceMemory: fp.deviceMemory,
    webglVendor: fp.webglVendor,
    webglRenderer: fp.webglRenderer,
    canvasNoiseSeed: fp.canvasNoiseSeed,
    audioNoiseSeed: fp.audioNoiseSeed,
  });
  return `
(() => {
  const FP = ${data};
  try { Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => false }); } catch (e) {}
  try { Object.defineProperty(Navigator.prototype, 'languages', { get: () => FP.languages }); } catch (e) {}
  try { Object.defineProperty(Navigator.prototype, 'platform', { get: () => FP.platform }); } catch (e) {}
  try { Object.defineProperty(Navigator.prototype, 'hardwareConcurrency', { get: () => FP.hardwareConcurrency }); } catch (e) {}
  try { Object.defineProperty(Navigator.prototype, 'deviceMemory', { get: () => FP.deviceMemory }); } catch (e) {}
  try {
    const plugins = [
      { name: 'PDF Viewer', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
      { name: 'Chrome PDF Viewer', filename: 'internal-pdf-viewer', description: '' },
    ];
    Object.defineProperty(Navigator.prototype, 'plugins', { get: () => plugins });
  } catch (e) {}
  try {
    const getParam = WebGLRenderingContext.prototype.getParameter;
    WebGLRenderingContext.prototype.getParameter = function(p) {
      if (p === 37445) return FP.webglVendor;
      if (p === 37446) return FP.webglRenderer;
      return getParam.call(this, p);
    };
    if (typeof WebGL2RenderingContext !== 'undefined') {
      const getParam2 = WebGL2RenderingContext.prototype.getParameter;
      WebGL2RenderingContext.prototype.getParameter = function(p) {
        if (p === 37445) return FP.webglVendor;
        if (p === 37446) return FP.webglRenderer;
        return getParam2.call(this, p);
      };
    }
  } catch (e) {}
  try {
    if (!window.chrome) {
      window.chrome = { runtime: {}, app: {}, csi: () => ({}), loadTimes: () => ({}) };
    }
  } catch (e) {}
})();
`;
}
