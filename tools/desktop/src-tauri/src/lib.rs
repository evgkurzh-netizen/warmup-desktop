//! Warmup desktop shell.
//!
//! Flow:
//!  1. On startup we check the OS keychain for the owner API token AND whether
//!     Playwright's Chromium runtime is installed.
//!  2. If either is missing we open a small local `welcome.html` window that:
//!       * collects the token and writes it to the keychain (commands
//!         `save_owner_token`, `welcome_init`); and
//!       * runs the capture sidecar in `--install-chromium` mode, surfacing
//!         live log lines to the welcome page via the `setup:chromium-log`
//!         event (`install_chromium` command).
//!     Once both are done the user clicks "Continue" → `finish_welcome`
//!     command closes the welcome window and opens the main one.
//!  3. The main window loads `https://warm-acc.fvds.ru/`. An
//!     `initialization_script` runs BEFORE any page script and:
//!       * wraps `window.fetch` to attach `x-api-key: <token>` (sourced from
//!         the keychain, held only in the JS closure — never written to
//!         localStorage); and
//!       * exposes a frozen `window.__YWK_DESKTOP__` object with `capture`,
//!         `setToken` and `_onEvent` hooks the dashboard uses.
//!  4. Capture is triggered by the dashboard navigating to
//!     `ywk-desktop://capture?account=<id>` — intercepted by
//!     `on_navigation`. The sidecar is spawned via `tauri-plugin-shell` and
//!     each JSON event it prints to stdout is forwarded to the main window
//!     via `webview.eval("window.__YWK_DESKTOP__._onEvent(…)")`.
//!  5. Any other navigation (third-party links, OAuth popups, etc.) is
//!     opened in the user's default browser — the webview is locked to the
//!     dashboard origin so the injected token can never leak.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{
    AppHandle, Emitter, Manager, Runtime, State, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_shell::{
    process::{CommandChild, CommandEvent},
    ShellExt,
};
use tokio::sync::Mutex;
use url::Url;

const APP_BASE_URL: &str = "https://warm-acc.fvds.ru/";
const ALLOWED_HOST: &str = "warm-acc.fvds.ru";
const KEYRING_SERVICE: &str = "ru.fvds.warmacc.desktop";
const KEYRING_USER: &str = "owner-token";
const KEYRING_BASIC_USER_ITEM: &str = "basic-auth-user";
const KEYRING_BASIC_PASS_ITEM: &str = "basic-auth-pass";
const DEEP_LINK_SCHEME: &str = "ywk-desktop";

#[derive(Default)]
struct SetupState {
    /// `true` after we have successfully installed Chromium in this session.
    chromium_ready: Mutex<bool>,
}

/// Tracks the live capture sidecar processes by account id so we can (a)
/// reject duplicate spawns when the user double-clicks Capture and (b) kill
/// the whole tree when the desktop app exits — otherwise orphaned Chromium
/// windows keep popping up after the user closes Warmup.
/// Slot in the capture registry. `Reserved` means a spawn is in flight —
/// kept distinct from `Live` so the duplicate-capture guard is atomic with
/// respect to `Shell::spawn()`, and so `ExitRequested` doesn't try to kill a
/// child that doesn't exist yet.
enum CaptureSlot {
    Reserved,
    Live(CommandChild),
}

#[derive(Default)]
struct CaptureRegistry {
    children: std::sync::Mutex<HashMap<String, CaptureSlot>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(SetupState::default()))
        .manage(Arc::new(CaptureRegistry::default()))
        .invoke_handler(tauri::generate_handler![
            welcome_init,
            save_owner_token,
            save_basic_auth,
            get_owner_token_jit,
            install_chromium,
            finish_welcome,
            capture_account,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let needs_welcome = read_token().is_none()
                || read_basic_auth().is_none()
                || !chromium_installed();
            if needs_welcome {
                open_welcome_window(&handle)?;
            } else {
                open_main_window(&handle)?;
            }

            // Check for updates in the background. Failures are non-fatal.
            #[cfg(not(debug_assertions))]
            {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(err) = check_for_updates(&h).await {
                        log::warn!("updater check failed: {err}");
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // Reap any sidecar processes that are still running so the
                // user doesn't see orphaned Chromium windows after closing
                // Warmup.
                if let Some(reg) = app_handle.try_state::<Arc<CaptureRegistry>>() {
                    let mut map = reg.children.lock().unwrap();
                    for (account, slot) in map.drain() {
                        if let CaptureSlot::Live(child) = slot {
                            log::info!("killing capture sidecar for {account} on exit");
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Token storage (OS keychain only — never written to disk or localStorage)
// ---------------------------------------------------------------------------

fn read_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    let value = entry.get_password().ok()?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn write_token(token: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    entry.set_password(token)
}

fn read_keychain_item(item: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, item).ok()?;
    let value = entry.get_password().ok()?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn write_keychain_item(item: &str, value: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, item)?;
    entry.set_password(value)
}

/// Read Basic auth user+password pair from keychain, if both are present.
fn read_basic_auth() -> Option<(String, String)> {
    let user = read_keychain_item(KEYRING_BASIC_USER_ITEM)?;
    let pass = read_keychain_item(KEYRING_BASIC_PASS_ITEM)?;
    Some((user, pass))
}

// ---------------------------------------------------------------------------
// Chromium presence check
// ---------------------------------------------------------------------------

/// Look in Playwright's standard cache directory for any `chromium-*` install.
fn chromium_installed() -> bool {
    let dir = playwright_cache_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("chromium-")
        {
            return true;
        }
    }
    false
}

fn playwright_cache_dir() -> std::path::PathBuf {
    if let Ok(custom) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        return std::path::PathBuf::from(custom);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_home() {
            return home.join("Library/Caches/ms-playwright");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return std::path::PathBuf::from(local).join("ms-playwright");
        }
    }
    std::path::PathBuf::from(".")
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

// ---------------------------------------------------------------------------
// Window builders
// ---------------------------------------------------------------------------

fn open_welcome_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    WebviewWindowBuilder::new(
        app,
        "welcome",
        WebviewUrl::App("welcome.html".into()),
    )
    .title("Warmup — setup")
    .inner_size(560.0, 620.0)
    .resizable(false)
    .build()
}

fn open_main_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    let init_script = build_init_script();
    let mut url = Url::parse(APP_BASE_URL).expect("APP_BASE_URL is valid");

    // If the user provided Basic auth credentials during welcome (stored in
    // keychain), embed them into the URL as `https://user:pass@host/`. Both
    // WKWebView (macOS) and WebView2 (Windows) extract these credentials and
    // cache them for the origin, so subsequent requests within the same origin
    // also send `Authorization: Basic ...` without further prompts.
    // `Url::set_username/set_password` performs URL-encoding for us.
    if let Some((user, pass)) = read_basic_auth() {
        let _ = url.set_username(&user);
        let _ = url.set_password(Some(&pass));
    }

    // Embed the owner token into the URL fragment so the init script can
    // pick it up synchronously without an `invoke()` call (which WebKit
    // blocks as mixed content on https origins via the `ipc://` scheme).
    // The fragment never leaves the client (browsers do not send it to
    // the server) and the init script wipes it from the URL via
    // history.replaceState before any page code runs.
    let token = read_token().unwrap_or_default();
    if !token.is_empty() {
        let encoded: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
        url.set_fragment(Some(&format!("__ywk_t={encoded}")));
    }

    let nav_app = app.clone();
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Warmup")
        .inner_size(1320.0, 860.0)
        .min_inner_size(960.0, 600.0)
        .initialization_script(&init_script)
        .on_navigation(move |url| handle_navigation(&nav_app, url))
        .build()
}

// ---------------------------------------------------------------------------
// Commands invoked from welcome.html
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WelcomeInit {
    has_token: bool,
    has_basic_auth: bool,
}

#[tauri::command]
async fn welcome_init() -> WelcomeInit {
    WelcomeInit {
        has_token: read_token().is_some(),
        has_basic_auth: read_basic_auth().is_some(),
    }
}

#[tauri::command]
async fn save_owner_token(token: String) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("Token is empty".into());
    }
    write_token(trimmed).map_err(|e| format!("keychain: {e:?}"))
}

/// Read the current owner token from the OS keychain. Called by the
/// webview's init script on every page load so that token changes
/// (welcome flow OR in-page setToken + reload) are picked up without
/// needing an app restart. Returns "" if no token is stored.
#[tauri::command]
async fn get_owner_token_jit() -> String {
    read_token().unwrap_or_default()
}

#[tauri::command]
async fn save_basic_auth(user: String, pass: String) -> Result<(), String> {
    let user = user.trim();
    if user.is_empty() {
        return Err("Username is empty".into());
    }
    if pass.is_empty() {
        return Err("Password is empty".into());
    }
    write_keychain_item(KEYRING_BASIC_USER_ITEM, user)
        .map_err(|e| format!("keychain (user): {e:?}"))?;
    write_keychain_item(KEYRING_BASIC_PASS_ITEM, &pass)
        .map_err(|e| format!("keychain (pass): {e:?}"))?;
    Ok(())
}

#[tauri::command]
async fn install_chromium<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<SetupState>>,
) -> Result<(), String> {
    if *state.chromium_ready.lock().await {
        return Ok(());
    }

    let shell = app.shell();
    let (node_path, capture_js) = resolve_capture_paths(&app)?;
    let cmd = shell
        .command(node_path.to_string_lossy().to_string())
        .args([
            capture_js.to_string_lossy().to_string(),
            "--install-chromium".to_string(),
        ]);

    let (mut rx, _child) = cmd.spawn().map_err(|e| e.to_string())?;
    let mut last_line = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => {
                let text =
                    String::from_utf8_lossy(&line).trim().to_string();
                if !text.is_empty() {
                    let _ = app.emit("setup:chromium-log", &text);
                    last_line = text;
                }
            }
            CommandEvent::Terminated(payload) => {
                if payload.code.unwrap_or(0) != 0 {
                    return Err(format!(
                        "Chromium install failed: {last_line}"
                    ));
                }
            }
            _ => {}
        }
    }
    *state.chromium_ready.lock().await = true;
    Ok(())
}

#[tauri::command]
async fn finish_welcome<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    open_main_window(&app).map_err(|e| e.to_string())?;
    if let Some(welcome) = app.get_webview_window("welcome") {
        let _ = welcome.close();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Init script (runs before any page JS on the main webview)
// ---------------------------------------------------------------------------

fn build_init_script() -> String {
    // Token transport: the Rust side embeds the keychain token into the
    // initial URL as `#__ywk_t=<value>`. The fragment never reaches the
    // server. This init script reads it synchronously and immediately
    // strips it from the URL via history.replaceState before any page
    // code runs. The value is mirrored into sessionStorage so a manual
    // reload (Cmd+R) keeps working without a fresh fragment.
    //
    // We deliberately avoid `invoke("get_owner_token_jit")` here:
    // WebKit blocks the `ipc://localhost/...` custom protocol as mixed
    // content on https origins, and Tauri's postMessage fallback is not
    // always available before the fetch wrapper needs the token.
    //
    // For in-session updates (dashboard's API-token entry → set-token
    // deep-link), the Rust handler evals `__YWK_TOKEN_UPDATE__(value)`
    // directly into this window, so no reload is needed at all.
    format!(
        r##"
(function() {{
  var ORIGIN = window.location.origin;
  var state = {{ token: "" }};

  // 1) Pull token from the URL hash if Rust embedded it there.
  try {{
    var hash = window.location.hash || "";
    if (hash.charAt(0) === "#") hash = hash.slice(1);
    var params = new URLSearchParams(hash);
    var t = params.get("__ywk_t");
    if (t) {{
      state.token = t;
      try {{ sessionStorage.setItem("__ywk_t", t); }} catch (e) {{}}
    }}
  }} catch (e) {{}}

  // 2) Fall back to sessionStorage on a plain reload.
  if (!state.token) {{
    try {{
      var s = sessionStorage.getItem("__ywk_t");
      if (s) state.token = s;
    }} catch (e) {{}}
  }}

  // 3) Strip Basic-auth credentials AND the token fragment from the URL
  //    so the page never sees them in document.URL / location.href.
  try {{
    var loc = window.location;
    var needsRewrite = loc.username || loc.password ||
      (loc.hash && loc.hash.indexOf("__ywk_t=") >= 0);
    if (needsRewrite) {{
      // Preserve any other fragment params the page might use.
      var cleanHash = "";
      try {{
        var hp = new URLSearchParams((loc.hash || "").replace(/^#/, ""));
        hp.delete("__ywk_t");
        var rest = hp.toString();
        if (rest) cleanHash = "#" + rest;
      }} catch (e) {{}}
      var clean = loc.pathname + loc.search + cleanHash;
      window.history.replaceState(null, "", clean);
    }}
  }} catch (e) {{}}

  // Exposed to Rust via WebviewWindow::eval — see set-token handler.
  window.__YWK_TOKEN_UPDATE__ = function(value) {{
    state.token = value || "";
    try {{
      if (state.token) sessionStorage.setItem("__ywk_t", state.token);
      else sessionStorage.removeItem("__ywk_t");
    }} catch (e) {{}}
  }};

  // -- Diagnostic overlay --------------------------------------------------
  // Always-on status bar at the bottom of the window. The user reported
  // the dashboard renders empty and they cannot reliably copy the WebKit
  // inspector console, so we surface the key facts (token presence and
  // the most recent /api/* response) directly on the page.
  function tokenLabel() {{
    var tok = state.token || "";
    if (!tok) return "EMPTY";
    if (tok.length <= 8) return "***";
    return tok.slice(0, 4) + "..." + tok.slice(-4) + " (" + tok.length + ")";
  }}
  function ensureDiag() {{
    if (!document.body) return;
    if (document.getElementById("__ywk_diag__")) return;
    var wrap = document.createElement("div");
    wrap.id = "__ywk_diag__";
    wrap.style.cssText = [
      "position:fixed", "bottom:0", "left:0", "right:0",
      "z-index:2147483647", "pointer-events:auto"
    ].join(";");
    var log = document.createElement("div");
    log.id = "__ywk_diag_log__";
    log.style.cssText = [
      "background:rgba(0,0,0,0.85)", "color:#0f0",
      "font:11px/1.4 ui-monospace,monospace", "padding:4px 8px",
      "max-height:160px", "overflow-y:auto",
      "border-top:1px solid #333", "white-space:pre-wrap"
    ].join(";");
    var bar = document.createElement("div");
    bar.style.cssText = [
      "background:rgba(0,0,0,0.85)", "color:#0f0",
      "font:11px/1.4 ui-monospace,monospace", "padding:4px 8px",
      "display:flex", "gap:16px", "justify-content:space-between",
      "border-top:1px solid #333"
    ].join(";");
    bar.innerHTML =
      '<span id="__ywk_diag_v__">v0.1.36</span>' +
      '<span id="__ywk_diag_token__">Token: ?</span>' +
      '<span id="__ywk_diag_x__" style="cursor:pointer;color:#fff" title="Hide">x</span>';
    wrap.appendChild(log);
    wrap.appendChild(bar);
    document.body.appendChild(wrap);
    var x = document.getElementById("__ywk_diag_x__");
    if (x) x.onclick = function() {{ wrap.style.display = "none"; }};
    updateDiagToken();
  }}
  function updateDiagToken() {{
    var t = document.getElementById("__ywk_diag_token__");
    if (t) t.textContent = "Token: " + tokenLabel();
  }}
  function pushDiagLine(text, color) {{
    var l = document.getElementById("__ywk_diag_log__");
    if (!l) return;
    var line = document.createElement("div");
    if (color) line.style.color = color;
    var d = new Date();
    var ts = ("0"+d.getHours()).slice(-2)+":"+("0"+d.getMinutes()).slice(-2)+":"+("0"+d.getSeconds()).slice(-2);
    line.textContent = ts + " " + text;
    l.appendChild(line);
    while (l.childNodes.length > 16) l.removeChild(l.firstChild);
    l.scrollTop = l.scrollHeight;
  }}
  function setDiagLast(method, urlPath, status) {{
    var ok = (typeof status === "number" && status >= 200 && status < 300);
    pushDiagLine(method + " " + urlPath + " -> " + status, ok ? "#0f0" : "#f55");
  }}
  if (document.readyState === "loading") {{
    document.addEventListener("DOMContentLoaded", ensureDiag);
  }} else {{
    ensureDiag();
  }}

  // Wrap __YWK_TOKEN_UPDATE__ to refresh diag label on every change.
  var _origUpdate = window.__YWK_TOKEN_UPDATE__;
  window.__YWK_TOKEN_UPDATE__ = function(v) {{
    try {{ _origUpdate(v); }} catch (e) {{}}
    try {{ updateDiagToken(); }} catch (e) {{}}
  }};

  var orig = window.fetch.bind(window);
  window.fetch = function(input, init) {{
    init = init || {{}};
    var method = (init && init.method) || (input && input.method) || "GET";
    method = String(method).toUpperCase();
    var pathForDiag = "";
    try {{
      var urlStr = typeof input === "string"
        ? input
        : (input && input.url) || String(input);
      // Resolve against the clean origin -- the document's baseURI still
      // contains the Basic-auth userinfo from the initial navigation,
      // which makes WebKit reject every fetch with
      // "URL is not valid or contains user credentials".
      var u = new URL(urlStr, ORIGIN + "/");
      if (u.username || u.password) {{
        u.username = "";
        u.password = "";
      }}
      var cleanUrl = u.toString();
      if (u.origin === ORIGIN && u.pathname.indexOf("/api/") === 0) {{
        pathForDiag = u.pathname;
        var headers = new Headers(
          init.headers || (input && input.headers ? input.headers : undefined)
        );
        if (state.token && !headers.has("x-api-key")) {{
          headers.set("x-api-key", state.token);
        }}
        init = Object.assign({{}}, init, {{ headers: headers }});
        input = cleanUrl;
      }} else if (cleanUrl !== urlStr) {{
        // Non-/api/* request whose URL also needs cleaning.
        if (typeof input !== "string" && input && input.url) {{
          // Request object -- rebuild it against the clean URL.
          try {{ input = new Request(cleanUrl, input); }} catch (e) {{ input = cleanUrl; }}
        }} else {{
          input = cleanUrl;
        }}
      }}
    }} catch (e) {{}}
    var p = orig(input, init);
    if (pathForDiag) {{
      p.then(function(r) {{
        try {{
          setDiagLast(method, pathForDiag,
            r.status + (state.token ? "" : " (NO TOKEN)"));
        }} catch (e) {{}}
      }}).catch(function(err) {{
        try {{ setDiagLast(method, pathForDiag, "ERR " + (err && err.message ? err.message : err)); }} catch (e) {{}}
      }});
    }}
    return p;
  }};

  var listeners = [];
  window.__YWK_DESKTOP__ = Object.freeze({{
    version: "0.1.36",
    hasToken: true,
    capture: function(accountId) {{
      if (!accountId) return;
      try {{ pushDiagLine("INV capture_account " + accountId.slice(0,8) + " calling", "#ff0"); }} catch (e) {{}}
      // Prefer the standard Tauri invoke channel -- in Tauri 2 the global
      // bridge falls back to postMessage when ipc:// is blocked by mixed
      // content rules on https:// origins. The iframe deep-link path
      // turned out unreliable for custom schemes in subframes.
      try {{
        var core = (window.__TAURI__ && window.__TAURI__.core) ||
                   (window.__TAURI_INTERNALS__ ? window.__TAURI_INTERNALS__ : null);
        if (core && typeof core.invoke === "function") {{
          core.invoke("capture_account", {{ accountId: accountId }}).then(function() {{
            try {{ pushDiagLine("INV capture_account " + accountId.slice(0,8) + " ok", "#0f0"); }} catch (e) {{}}
          }}).catch(function(err) {{
            try {{ pushDiagLine("[invoke error] " + (err && err.message ? err.message : err), "#f55"); }} catch (e) {{}}
          }});
          return;
        }}
      }} catch (e) {{}}
      try {{ pushDiagLine("[error] window.__TAURI__ unavailable", "#f55"); }} catch (e) {{}}
    }},
    setToken: function(value) {{
      if (!value) return;
      // Pre-apply locally so the very next fetch already carries it,
      // even before the deep-link round-trip lands.
      try {{ window.__YWK_TOKEN_UPDATE__(value); }} catch (e) {{}}
      var iframe = document.createElement("iframe");
      iframe.style.display = "none";
      iframe.src =
        "{scheme}://set-token?value=" + encodeURIComponent(value);
      document.body.appendChild(iframe);
      setTimeout(function() {{
        try {{ iframe.remove(); }} catch (e) {{}}
      }}, 500);
    }},
    onCaptureEvent: function(cb) {{
      listeners.push(cb);
      return function() {{
        var i = listeners.indexOf(cb);
        if (i >= 0) listeners.splice(i, 1);
      }};
    }},
    _onEvent: function(ev) {{
      // Mirror to the diagnostic overlay so the user can see capture
      // sidecar progress / errors even when the dashboard's own toast
      // UI does not show them.
      try {{
        if (ev && ev.type) {{
          var summary = "[" + ev.type + "]";
          if (ev.message) summary += " " + ev.message;
          else if (ev.cookie_count != null) summary += " cookies=" + ev.cookie_count;
          else if (ev.label) summary += " " + ev.label;
          var color = ev.type === "error" ? "#f55"
            : (ev.type === "done" ? "#0f0" : "#ff0");
          pushDiagLine(summary, color);
        }}
      }} catch (e) {{}}
      for (var i = 0; i < listeners.length; i++) {{
        try {{ listeners[i](ev); }} catch (e) {{}}
      }}
    }}
  }});
}})();
"##,
        scheme = DEEP_LINK_SCHEME,
    )
}

// ---------------------------------------------------------------------------
// Navigation interception
// ---------------------------------------------------------------------------

/// Fire-and-forget command exposed to the dashboard via `window.__TAURI__`
/// (postMessage transport). Replaces the unreliable iframe deep-link path
/// for triggering the capture sidecar.
#[tauri::command]
async fn capture_account<R: Runtime>(
    app: AppHandle<R>,
    account_id: String,
) -> Result<(), String> {
    push_event(
        &app,
        &CaptureEvent::Progress {
            message: format!("invoke received account={account_id}"),
        },
    );
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(err) = run_capture(&handle, &account_id).await {
            log::error!("capture failed: {err}");
            push_event(
                &handle,
                &CaptureEvent::Error {
                    message: err.to_string(),
                },
            );
        }
    });
    Ok(())
}

fn handle_navigation<R: Runtime>(app: &AppHandle<R>, url: &Url) -> bool {
    if url.scheme() == DEEP_LINK_SCHEME {
        let host = url.host_str().unwrap_or_default();
        let query: HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        match host {
            "capture" => {
                if let Some(account) = query.get("account") {
                    let account = account.clone();
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = run_capture(&handle, &account).await
                        {
                            log::error!("capture failed: {err}");
                            push_event(
                                &handle,
                                &CaptureEvent::Error {
                                    message: err.to_string(),
                                },
                            );
                        }
                    });
                }
            }
            "set-token" => {
                if let Some(value) = query.get("value") {
                    if !value.is_empty() {
                        if let Err(err) = write_token(value) {
                            log::warn!(
                                "failed to persist token: {err:?}"
                            );
                        } else if let Some(win) =
                            app.get_webview_window("main")
                        {
                            // Push the new value into the running page
                            // so the next fetch picks it up without any
                            // reload or window rebuild. Init script
                            // exposes __YWK_TOKEN_UPDATE__ for this.
                            let js = format!(
                                "if(window.__YWK_TOKEN_UPDATE__)window.__YWK_TOKEN_UPDATE__({});",
                                serde_json::Value::String(value.to_string())
                            );
                            let _ = win.eval(&js);
                        }
                    }
                }
            }
            _ => log::warn!("unknown deep-link host: {host}"),
        }
        return false;
    }

    let allowed = matches!(url.scheme(), "https" | "http")
        && url.host_str() == Some(ALLOWED_HOST);
    if allowed {
        return true;
    }

    // Anything else: open in the OS browser and cancel the navigation so
    // the token never reaches a foreign origin.
    let s = url.to_string();
    if let Err(err) = app.shell().open(s.clone(), None) {
        log::warn!("shell open {s} failed: {err}");
    }
    false
}

// ---------------------------------------------------------------------------
// Capture sidecar
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CaptureEvent {
    Started {
        account_id: String,
        label: String,
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        has_proxy: Option<bool>,
    },
    Progress {
        message: String,
    },
    Done {
        account_id: String,
        cookie_count: u32,
    },
    Error {
        message: String,
    },
}

/// Resolve the bundled Node.js binary and capture.cjs entrypoint paths
/// inside Tauri's resource directory. The build.mjs script stages both
/// under `resources/sidecar/` so the structure at runtime is:
///   <resource_dir>/resources/sidecar/node[.exe]
///   <resource_dir>/resources/sidecar/capture.cjs
///   <resource_dir>/resources/sidecar/node_modules/...
fn resolve_capture_paths<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let base = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir unavailable: {e}"))?;
    let sidecar = base.join("resources").join("sidecar");
    let node_name = if cfg!(target_os = "windows") {
        "node.exe"
    } else {
        "node"
    };
    let node_path = sidecar.join(node_name);
    let capture_js = sidecar.join("capture.cjs");
    if !node_path.exists() {
        return Err(format!("bundled node missing at {}", node_path.display()));
    }
    if !capture_js.exists() {
        return Err(format!("capture.cjs missing at {}", capture_js.display()));
    }
    Ok((node_path, capture_js))
}

async fn run_capture<R: Runtime>(
    app: &AppHandle<R>,
    account_id: &str,
) -> Result<(), String> {
    push_event(app, &CaptureEvent::Progress { message: "run_capture: reading token".into() });
    let token = read_token().ok_or_else(|| {
        "No owner token in keychain — re-run first-launch setup.".to_string()
    })?;

    let shell = app.shell();
    push_event(app, &CaptureEvent::Progress { message: "run_capture: resolving sidecar".into() });
    let (node_path, capture_js) = resolve_capture_paths(app)?;
    let cmd = shell
        .command(node_path.to_string_lossy().to_string())
        .args([
            capture_js.to_string_lossy().to_string(),
            "--base-url".to_string(),
            APP_BASE_URL.trim_end_matches('/').to_string(),
            "--token".to_string(),
            token.clone(),
            "--account".to_string(),
            account_id.to_string(),
            "--emit-json".to_string(),
        ]);
        // NOTE: Do NOT set NODE_OPTIONS=--jitless here. It disables V8 JIT
        // *and* WebAssembly, which Node's bundled undici requires to compile
        // llhttp -- without WASM, the first fetch() call throws before any
        // output reaches us, and the process appears to exit silently with
        // code 0. The macOS-15 / Apple-Silicon CodeRange OOM is addressed by
        // shipping Node 22 as a bundled resource (see sidecar/build.mjs).

    // Reject a second concurrent capture for the same account — that's how
    // users ended up with 5+ Chromium windows opening on top of each other
    // by clicking Capture repeatedly while the first browser was still up.
    // Atomically reserve the slot BEFORE spawn so two concurrent invokes
    // can't both pass the check-and-insert window.
    let registry = app.state::<Arc<CaptureRegistry>>().inner().clone();
    {
        let mut map = registry.children.lock().unwrap();
        if map.contains_key(account_id) {
            return Err(
                "A capture browser is already open for this account. Finish or close it first.".into(),
            );
        }
        map.insert(account_id.to_string(), CaptureSlot::Reserved);
    }

    push_event(app, &CaptureEvent::Progress { message: "run_capture: spawning".into() });
    let (mut rx, child) = match cmd.spawn() {
        Ok(pair) => pair,
        Err(e) => {
            // Clear the reservation so the next click can retry.
            let mut map = registry.children.lock().unwrap();
            map.remove(account_id);
            return Err(format!("spawn failed: {e}"));
        }
    };
    {
        let mut map = registry.children.lock().unwrap();
        map.insert(account_id.to_string(), CaptureSlot::Live(child));
    }
    push_event(app, &CaptureEvent::Progress { message: "run_capture: spawned, awaiting output".into() });
    let mut saw_error_event = false;
    let mut last_stderr = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            CommandEvent::Stdout(line) => {
                let text = String::from_utf8_lossy(&line).to_string();
                let trimmed = text.trim();
                if let Ok(parsed) =
                    serde_json::from_str::<CaptureEvent>(trimmed)
                {
                    if matches!(parsed, CaptureEvent::Error { .. }) {
                        saw_error_event = true;
                    }
                    push_event(app, &parsed);
                } else if !trimmed.is_empty() {
                    // Non-JSON stdout (e.g. install-chromium mode lines) —
                    // surface it as a progress event so we don't lose it.
                    push_event(
                        app,
                        &CaptureEvent::Progress {
                            message: format!("stdout: {trimmed}"),
                        },
                    );
                }
            }
            CommandEvent::Stderr(line) => {
                let text =
                    String::from_utf8_lossy(&line).trim().to_string();
                if !text.is_empty() {
                    log::warn!("[capture stderr] {text}");
                    // Also forward to the overlay so users can see why the
                    // sidecar process is failing (missing Chromium, missing
                    // .node binding from pkg, etc.).
                    push_event(
                        app,
                        &CaptureEvent::Progress {
                            message: format!("stderr: {text}"),
                        },
                    );
                    last_stderr = text;
                }
            }
            CommandEvent::Terminated(payload) => {
                let code = payload.code.unwrap_or(0);
                let signal = payload.signal;
                push_event(
                    app,
                    &CaptureEvent::Progress {
                        message: format!(
                            "terminated code={code} signal={}",
                            signal
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "none".into()),
                        ),
                    },
                );
                let killed_by_signal = signal.is_some();
                if (code != 0 || killed_by_signal) && !saw_error_event {
                    let msg = if last_stderr.is_empty() {
                        format!("capture exited with code {code}")
                    } else {
                        format!("capture exited with code {code}: {last_stderr}")
                    };
                    push_event(
                        app,
                        &CaptureEvent::Error { message: msg },
                    );
                }
            }
            _ => {}
        }
    }
    // Always drop the registry entry — the child has either exited cleanly
    // or been killed; either way we want the next Capture click to be able
    // to spawn a fresh browser.
    {
        let mut map = registry.children.lock().unwrap();
        map.remove(account_id);
    }
    Ok(())
}

/// Forward a `CaptureEvent` to the main window's `window.__YWK_DESKTOP__._onEvent`.
fn push_event<R: Runtime>(app: &AppHandle<R>, ev: &CaptureEvent) {
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(payload) = serde_json::to_string(ev) {
            let _ = w.eval(format!(
                "window.__YWK_DESKTOP__ && window.__YWK_DESKTOP__._onEvent({payload});"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-updater
// ---------------------------------------------------------------------------

#[cfg(not(debug_assertions))]
async fn check_for_updates<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(());
    };
    log::info!("update available: {}", update.version);
    update
        .download_and_install(|_, _| {}, || log::info!("update installed"))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
