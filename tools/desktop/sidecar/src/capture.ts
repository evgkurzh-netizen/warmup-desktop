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
import { chromium, type Browser, type BrowserContext, type Page } from "playwright";

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

async function launch(
  ctx: LoginContext,
): Promise<{ browser: Browser; context: BrowserContext; page: Page }> {
  const browser = await chromium.launch({
    headless: false,
    args: [
      "--disable-blink-features=AutomationControlled",
      "--no-sandbox",
      "--disable-setuid-sandbox",
      `--lang=${ctx.fingerprint.languages[0] ?? "en-US"}`,
    ],
    proxy: ctx.proxy
      ? {
          server: `${ctx.proxy.type === "socks5" ? "socks5" : "http"}://${ctx.proxy.host}:${ctx.proxy.port}`,
          username: ctx.proxy.username ?? undefined,
          password: ctx.proxy.password ?? undefined,
        }
      : undefined,
  });
  const context = await browser.newContext({
    userAgent: ctx.fingerprint.userAgent,
    locale: ctx.fingerprint.languages[0] ?? "en-US",
    timezoneId: ctx.fingerprint.timezone,
    viewport: {
      width: ctx.fingerprint.screenWidth,
      height: ctx.fingerprint.screenHeight,
    },
    deviceScaleFactor: ctx.fingerprint.deviceScaleFactor,
    isMobile: ctx.fingerprint.device === "mobile",
    hasTouch: ctx.fingerprint.device === "mobile",
    extraHTTPHeaders: { "Accept-Language": ctx.fingerprint.languages.join(",") },
  });
  await context.addInitScript(stealthScript(ctx.fingerprint));
  const page = await context.newPage();
  return { browser, context, page };
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

  const { context, page, browser } = await launch(ctx);
  try {
    await page.goto(args.startUrl, { waitUntil: "commit", timeout: 90_000 });
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
    try {
      await context.close();
    } catch {
      /* ignore */
    }
    try {
      await browser.close();
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
