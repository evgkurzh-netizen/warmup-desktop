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
use tauri_plugin_shell::{process::CommandEvent, ShellExt};
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(SetupState::default()))
        .invoke_handler(tauri::generate_handler![
            welcome_init,
            save_owner_token,
            save_basic_auth,
            install_chromium,
            finish_welcome,
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
    let token = read_token().unwrap_or_default();
    let init_script = build_init_script(&token);
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
    let cmd = shell
        .sidecar("capture")
        .map_err(|e| format!("sidecar not bundled: {e}"))?
        .args(["--install-chromium"]);

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

fn build_init_script(token: &str) -> String {
    // The token is embedded into the script as an escaped JS string literal.
    // It lives only in the wrapper-closure variable below — NOT in
    // localStorage, sessionStorage, cookies, or anywhere else the page could
    // read it from.
    let token_literal =
        serde_json::Value::String(token.to_string()).to_string();
    format!(
        r#"
(function() {{
  var TOKEN = {token_literal};
  var ORIGIN = window.location.origin;

  // Strip embedded Basic-auth credentials from the document URL.
  // We navigated to `https://user:pass@host/` so the engine performs
  // the initial Basic auth, but the WHATWG Fetch spec rejects any
  // fetch() whose URL contains userinfo. After the first navigation
  // the engine caches credentials for the origin, so subsequent
  // requests work without userinfo in the URL.
  try {{
    var loc = window.location;
    if (loc.username || loc.password) {{
      var clean = loc.pathname + loc.search + loc.hash;
      window.history.replaceState(null, "", clean);
    }}
  }} catch (e) {{}}

  var orig = window.fetch.bind(window);
  window.fetch = function(input, init) {{
    init = init || {{}};
    try {{
      var urlStr = typeof input === "string"
        ? input
        : (input && input.url) || String(input);
      var u = new URL(urlStr, ORIGIN);
      if (TOKEN && u.origin === ORIGIN && u.pathname.indexOf("/api/") === 0) {{
        var headers = new Headers(
          init.headers || (input && input.headers ? input.headers : undefined)
        );
        if (!headers.has("x-api-key")) headers.set("x-api-key", TOKEN);
        init = Object.assign({{}}, init, {{ headers: headers }});
      }}
    }} catch (e) {{}}
    return orig(input, init);
  }};

  var listeners = [];
  window.__YWK_DESKTOP__ = Object.freeze({{
    version: "0.1.2",
    hasToken: !!TOKEN,
    capture: function(accountId) {{
      if (!accountId) return;
      window.location.href =
        "{scheme}://capture?account=" + encodeURIComponent(accountId);
    }},
    setToken: function(value) {{
      if (!value) return;
      var iframe = document.createElement("iframe");
      iframe.style.display = "none";
      iframe.src =
        "{scheme}://set-token?value=" + encodeURIComponent(value);
      document.body.appendChild(iframe);
      setTimeout(function() {{
        try {{ iframe.remove(); }} catch (e) {{}}
        window.location.reload();
      }}, 800);
    }},
    onCaptureEvent: function(cb) {{
      listeners.push(cb);
      return function() {{
        var i = listeners.indexOf(cb);
        if (i >= 0) listeners.splice(i, 1);
      }};
    }},
    _onEvent: function(ev) {{
      for (var i = 0; i < listeners.length; i++) {{
        try {{ listeners[i](ev); }} catch (e) {{}}
      }}
    }}
  }});
}})();
"#,
        token_literal = token_literal,
        scheme = DEEP_LINK_SCHEME,
    )
}

// ---------------------------------------------------------------------------
// Navigation interception
// ---------------------------------------------------------------------------

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

async fn run_capture<R: Runtime>(
    app: &AppHandle<R>,
    account_id: &str,
) -> Result<(), String> {
    let token = read_token().ok_or_else(|| {
        "No owner token in keychain — re-run first-launch setup.".to_string()
    })?;

    let shell = app.shell();
    let cmd = shell
        .sidecar("capture")
        .map_err(|e| format!("sidecar not bundled: {e}"))?
        .args([
            "--base-url",
            APP_BASE_URL.trim_end_matches('/'),
            "--token",
            &token,
            "--account",
            account_id,
            "--emit-json",
        ]);

    let (mut rx, _child) = cmd.spawn().map_err(|e| e.to_string())?;
    let mut saw_error_event = false;
    let mut last_stderr = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            CommandEvent::Stdout(line) => {
                let text = String::from_utf8_lossy(&line).to_string();
                if let Ok(parsed) =
                    serde_json::from_str::<CaptureEvent>(text.trim())
                {
                    if matches!(parsed, CaptureEvent::Error { .. }) {
                        saw_error_event = true;
                    }
                    push_event(app, &parsed);
                }
            }
            CommandEvent::Stderr(line) => {
                let text =
                    String::from_utf8_lossy(&line).trim().to_string();
                if !text.is_empty() {
                    log::warn!("[capture stderr] {text}");
                    last_stderr = text;
                }
            }
            CommandEvent::Terminated(payload) => {
                let code = payload.code.unwrap_or(0);
                if code != 0 && !saw_error_event {
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
