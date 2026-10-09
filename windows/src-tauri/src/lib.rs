// Coucou for Windows — app wiring and the commands the island calls.

mod claude;
mod files;
mod hooks;
mod integrations;
mod island;
mod log;
mod ollama;
mod openai;
mod pipe;
mod secrets;
mod settings;
mod store;
mod tools;
mod tray;
mod win_user;

/// For the integration tests in `tests/`, which live outside the crate and
/// cannot see a private module.
#[doc(hidden)]
pub use store::{testing_clients, testing_store};

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use claude::{ChatContext, ChatReply};
use files::DroppedFile;
use hooks::{HookPreview, HookStatus};
use island::{PollGate, ScreenInfo};
use pipe::Pending;
use settings::Settings;
use store::ChatStore;

/// Keeps spawned helpers from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub gate: Arc<PollGate>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootInfo {
    settings: Settings,
    screen: ScreenInfo,
    version: String,
    hook_path: String,
}

#[tauri::command]
fn boot(app: AppHandle, shared: State<Shared>) -> BootInfo {
    let mut settings = shared.settings.lock().unwrap().clone();
    // The real state of each agent's config wins over whatever we stored.
    settings.hooks_installed = hooks::status(hooks::AgentProvider::Claude).installed;
    settings.codex_hooks_installed = hooks::status(hooks::AgentProvider::Codex).installed;
    settings.kimi_hooks_installed = hooks::status(hooks::AgentProvider::Kimi).installed;
    settings = settings::with_all_agents(settings);
    let screen = island::screen_info(&app, &settings.screen);
    BootInfo {
        settings,
        screen,
        version: env!("CARGO_PKG_VERSION").to_string(),
        hook_path: settings::hook_exe_path().to_string_lossy().to_string(),
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, shared: State<Shared>, settings: Settings) {
    let (screen_changed, autostart_changed) = {
        let mut current = shared.settings.lock().unwrap();
        let screen_changed = current.screen != settings.screen;
        let autostart_changed = current.autostart != settings.autostart;
        *current = settings::with_all_agents(settings.clone());
        (screen_changed, autostart_changed)
    };
    if let Err(err) = settings::save(&settings) {
        eprintln!("[coucou] could not save settings: {err}");
    }
    if autostart_changed {
        let manager = app.autolaunch();
        let result = if settings.autostart { manager.enable() } else { manager.disable() };
        if let Err(err) = result {
            eprintln!("[coucou] autostart: {err}");
        }
    }
    if screen_changed {
        let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
        island::apply_geometry(&app, &settings.screen, collapsed);
    }
    // Keep the other window in step (island ⇄ settings window).
    let _ = app.emit("settings-changed", settings);
}

/// Hidden island → shrink the window to the invisible wake strip and park the
/// cursor poll; anything else → full panel and 60 Hz polling.
#[tauri::command]
fn set_collapsed(app: AppHandle, shared: State<Shared>, collapsed: bool) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    shared.gate.collapsed.store(collapsed, Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
    // The wake strip must always take the mouse, and a resize invalidates the flag.
    island::set_ignore_cursor(&app, false);
    shared.gate.forget_ignore_state();
    shared.gate.set_active(!collapsed);
}

/// The front end pushes the island shape; Rust decides click-through from it.
#[tauri::command]
fn set_island_rect(shared: State<Shared>, x: f64, y: f64, width: f64, height: f64) {
    shared.gate.set_rect(island::IslandRect { x, y, w: width, h: height });
}

#[tauri::command]
fn focus_window(app: AppHandle, focused: bool) {
    let Some(win) = island::window(&app) else { return };
    island::set_activating(&win, focused);
    if focused {
        let _ = win.set_focus();
    }
}

#[tauri::command]
fn reposition(app: AppHandle, shared: State<Shared>) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    let _ = Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// "Open terminal" opens the working folder in VS Code when `code` is on PATH,
/// and falls back to Explorer otherwise.
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    // No `cmd /C` anywhere near this. The path is a project folder chosen by
    // whoever is using Claude Code, and cmd would happily read `&`, `^` and `%`
    // in a folder name as syntax. Finding the launcher ourselves and handing the
    // path over as a separate argument keeps it a path.
    if let Some(code) = find_on_path("code") {
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
            cmd.arg(p);
        }
        if cmd.creation_flags(CREATE_NO_WINDOW).spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
        let _ = Command::new("explorer").arg(p).spawn();
    }
    false
}

/// Our own `where`: walks %PATH% against %PATHEXT%, no shell involved.
/// Rust quotes arguments correctly for `.cmd`/`.bat` targets since 1.77, so
/// spawning `code.cmd` directly is safe.
fn find_on_path(stem: &str) -> Option<std::path::PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&dirs) {
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let candidate = dir.join(format!("{stem}{}", ext.to_lowercase()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Tray → Pause. Paused means paused: the pollers stop talking to the network,
/// not just the island stopping showing things.
#[tauri::command]
fn set_paused(paused: bool) {
    integrations::set_paused(paused);
}

// ── Agent hooks (Claude Code, Codex, Cursor) ──────────────────────────────────

/// Installed state for every agent Coucou follows.
#[tauri::command]
fn hooks_status() -> Vec<HookStatus> {
    hooks::status_all()
}

/// Returns the diff the user has to look at before anything is written.
#[tauri::command]
fn hooks_preview(provider: String, install: bool) -> Result<HookPreview, String> {
    hooks::preview(provider_of(&provider)?, install)
}

/// Only ever called from an explicit click in the settings window.
#[tauri::command]
fn hooks_apply(
    app: AppHandle,
    shared: State<Shared>,
    provider: String,
    install: bool,
    fingerprint: String,
) -> Result<String, String> {
    let provider = provider_of(&provider)?;
    // The fingerprint comes from the preview the user actually looked at, so a
    // config that changed in between is refused rather than overwritten.
    let backup = hooks::write(provider, install, &fingerprint)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        match provider {
            hooks::AgentProvider::Claude => current.hooks_installed = install,
            hooks::AgentProvider::Codex => current.codex_hooks_installed = install,
            hooks::AgentProvider::Kimi => current.kimi_hooks_installed = install,
        }
        let _ = settings::save(&current);
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

fn provider_of(id: &str) -> Result<hooks::AgentProvider, String> {
    hooks::AgentProvider::parse(id).ok_or_else(|| format!("Unknown agent: {id}"))
}

#[tauri::command]
fn approval_decision(app: AppHandle, request_id: String, decision: String) {
    pipe::answer(&app, &request_id, &decision);
}

/// The island has the card on screen, so the long wait for a human may begin.
/// Until this arrives the relay only waits a few hundred milliseconds, which is
/// what stops a paused or unresponsive island from freezing Claude Code.
#[tauri::command]
fn approval_ack(app: AppHandle, request_id: String) {
    pipe::acknowledge(&app, &request_id);
}

/// Nobody can act on this request — the island is paused, or another card is
/// already up. Claude Code falls back to asking in the terminal immediately.
#[tauri::command]
fn approval_decline(app: AppHandle, request_id: String) {
    pipe::decline(&app, &request_id);
}

// ── Chat, files and secrets ───────────────────────────────────────────────────

/// What one chat turn is allowed to do, read from the settings on every send.
///
/// Built fresh each time so flipping the switch in the settings window takes
/// effect on the next question, with no restart.
pub fn chat_tool_ctx() -> claude::ToolCtx {
    let shell = settings::shell_settings();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    claude::ToolCtx {
        cwd,
        // An empty list, or the switch off, blocks every command: the allow-list
        // only means something once the user asked for shell access.
        allowed_shell: if shell.enabled {
            shell.allowed.clone()
        } else {
            Vec::new()
        },
        dry_run: shell.dry_run,
        tool_timeout_s: shell.timeout_s,
    }
}

/// Writes the shell rules the agent obeys.
///
/// Its own command rather than a field on `save_settings`, because these are
/// the settings that decide what the model is allowed to run: they get one
/// narrow door instead of riding along with every other preference.
#[tauri::command]
fn set_shell_settings(shared: State<'_, Shared>, shell: settings::ShellSettings) -> Result<(), String> {
    {
        let mut current = shared.settings.lock().unwrap();
        current.shell = shell.clone();
        settings::save(&current).map_err(|e| e.to_string())?;
    }
    settings::set_shell_settings(shell)
}

/// Which conversation the island is in.
///
/// A newtype rather than a bare Mutex<Option<String>> so the commands can
/// name it, and so the id has one obvious home: it is set on the first send and
/// replaced by chat_reset.
#[derive(Default)]
pub struct Session(pub Mutex<Option<String>>);

/// One chat turn. The API key and any file bytes stay on the Rust side.
///
/// The provider was picked once in the settings window; the island asks with
/// whatever that provider currently has selected.
///
/// The history is loaded from the session store, handed to the client, and
/// whatever the turn added is written back — so tool_use and tool_result blocks
/// are persisted with their structure, and the same conversation comes back
/// after a restart.
#[tauri::command]
async fn chat_send(
    shared: State<'_, Shared>,
    store: State<'_, ChatStore>,
    session: State<'_, Session>,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let (provider, config) = {
        let current = shared.settings.lock().unwrap();
        settings::chat_provider(&current)
    };
    let model = config.model.trim().to_string();
    let base_url = config.base_url;
    let model = remember_auto_model(&shared, &provider, model, &base_url).await;


    let session_id = current_session(&session)?;
    let mut history = store.load_session(&session_id)?;
    // Everything past this point is new, and gets saved.
    let already_saved = history.len();

    let outcome = match provider.as_str() {
        ollama::PROVIDER_OLLAMA => {
            ollama::send(&mut history, &base_url, &model, query, context).await
        }
        // Anything we do not have a dedicated client for speaks the
        // OpenAI-compatible shape, which is Codex, Kimi and every local gateway.
        ollama::PROVIDER_ANTHROPIC => {
            let ctx = chat_tool_ctx();
            claude::send(&mut history, &model, query, context, &ctx).await
        }
        _ => {
            let key = api_key_for(&provider)?;
            openai::send(&mut history, &model, &base_url, &key, query, context).await
        }
    };

    // A client that failed rolled its own turn back, so this saves whatever
    // actually reached the model — nothing new on a failure, the completed
    // exchange otherwise. Either way the store ends up describing what the
    // model has seen.
    for message in history.iter().skip(already_saved) {
        store.append(&session_id, &provider, message, None, None)?;
    }

    outcome
}

/// The model to use when settings has none, as far as the daemon can say.
///
/// Returns None whenever the answer should stay empty: another provider, a
/// model that was already chosen, or a daemon that is down or has nothing
/// local. The caller then leaves it to the provider to complain, which says
/// more about a daemon that is not running than this could.
pub(crate) async fn auto_model(
    provider: &str,
    model: &str,
    base_url: &str,
) -> Option<String> {
    if !model.trim().is_empty() || provider != ollama::PROVIDER_OLLAMA {
        return None;
    }
    ollama::preferred_model(base_url).await
}

/// Fills in a model that was never chosen, and remembers it.
///
/// Ollama is the one provider with no default: the name only exists once the
/// model has been pulled. Choosing Ollama in settings is therefore not enough
/// on its own — the chat would answer "no model selected" until somebody went
/// and pressed Detect models in the Ollama panel. So when there is nothing to
/// go on, ask the daemon which local model it has and write that down.
///
/// A model the user did choose is never touched: this only fills an empty one.
/// Returns the model to use, which is the input unchanged when nothing was
/// filled in — the caller then gets the provider's own error, which says more
/// about a daemon that is down than this could.
async fn remember_auto_model(
    shared: &Shared,
    provider: &str,
    model: String,
    base_url: &str,
) -> String {
    if !model.is_empty() || provider != ollama::PROVIDER_OLLAMA {
        return model;
    }
    let Some(found) = auto_model(provider, &model, base_url).await else {
        return model;
    };
    crate::log::line(format!("ollama: usando {found}"));
    let snapshot = apply_auto_model(shared, base_url, &found);
    if let Err(err) = settings::save(&snapshot) {
        // The chat answers either way; it just would not be remembered next
        // time, and refusing to answer over a preference file would be a
        // worse trade.
        eprintln!("[coucou] could not save settings: {err}");
    }
    found
}

/// Writes the auto-chosen model into the running app and hands back the
/// snapshot to persist.
///
/// Separate from the write so the in-memory part can be checked without going
/// anywhere near the user's own settings.json.
fn apply_auto_model(
    shared: &Shared,
    base_url: &str,
    found: &str,
) -> settings::Settings {
    let mut current = shared.settings.lock().unwrap();
    match current.providers.get_mut(ollama::PROVIDER_OLLAMA) {
        Some(entry) => entry.model = found.to_string(),
        None => {
            current.providers.insert(
                ollama::PROVIDER_OLLAMA.to_string(),
                settings::ProviderSettings {
                    model: found.to_string(),
                    base_url: base_url.to_string(),
                },
            );
        }
    }
    current.clone()
}

/// The session the app is talking to, opened on first use.
fn current_session(session: &State<'_, Session>) -> Result<String, String> {
    let mut guard = session.0.lock().unwrap();
    if let Some(id) = guard.as_ref() {
        return Ok(id.clone());
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let store = ChatStore::open()?;
    let id = store.resume_or_create(&cwd)?;
    *guard = Some(id.clone());
    Ok(id)
}

/// The key for one provider, from the Credential Manager.
///
/// Claude's key predates the per-provider names and is still read as a fallback,
/// so an existing installation keeps working without anybody touching anything.
fn api_key_for(provider: &str) -> Result<String, String> {
    if let Some(key) = secrets::get(&settings::api_key_name(provider)) {
        return Ok(key);
    }
    if provider == ollama::PROVIDER_ANTHROPIC {
        if let Some(key) = secrets::get("anthropic-api-key") {
            return Ok(key);
        }
    }
    Err(format!("No API key for {provider}. Open settings."))
}

/// Starts a new conversation and returns its id.
///
/// The old one is kept: a new chat is a new session, not a deletion, so
/// anything said before is still there to search for.
#[tauri::command]
fn chat_reset(
    store: State<'_, ChatStore>,
    session: State<'_, Session>,
) -> Result<String, String> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = store.new_session(&cwd)?;
    *session.0.lock().unwrap() = Some(id.clone());
    Ok(id)
}

/// The saved conversations, most recent first.
#[tauri::command]
fn chat_sessions(store: State<'_, ChatStore>) -> Result<Vec<SessionInfo>, String> {
    let sessions = store.list_sessions()?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionInfo {
            id: s.id,
            title: s.title,
            cwd: s.cwd,
            created_at: s.created_at,
            last_seen: s.last_seen,
        })
        .collect())
}

/// Full-text search across every saved conversation.
#[tauri::command]
fn chat_search(store: State<'_, ChatStore>, query: String) -> Result<Vec<SearchHit>, String> {
    let hits = store.search_fts(&query, 50)?;
    Ok(hits
        .into_iter()
        .map(|h| SearchHit {
            session_id: h.session_id,
            role: h.role,
            text: store::excerpt(&h.text, &query, 160),
        })
        .collect())
}

/// One saved conversation, for the island to list.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    id: String,
    title: Option<String>,
    cwd: String,
    created_at: i64,
    last_seen: i64,
}

/// One search result, with the text already cut around the match.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    session_id: String,
    role: String,
    text: String,
}

/// The models the local Ollama daemon has. Called from the settings window only,
/// so a daemon that is not running costs nothing while the island is hidden.
#[tauri::command]
async fn ollama_models(shared: State<'_, Shared>) -> Result<Vec<ollama::LocalModel>, String> {
    // The Ollama panel's own address, not the legacy top-level one: a daemon the
    // user moved in that panel would otherwise be detected against the wrong
    // host, and the list would come back empty for no visible reason.
    let base_url = {
        let current = shared.settings.lock().unwrap();
        let (_, config) = settings::chat_provider(&current);
        if config.base_url.trim().is_empty() {
            ollama::DEFAULT_BASE_URL.to_string()
        } else {
            config.base_url.clone()
        }
    };
    ollama::models(&base_url).await
}

/// Copies a dropped file into the inbox and reports its name back.
#[tauri::command]
fn ingest_file(path: String) -> Result<DroppedFile, String> {
    files::ingest(&path)
}

/// The island may only ask whether a key exists — never read it.
#[tauri::command]
fn secret_present(key: String) -> bool {
    secrets::present(&key)
}

#[tauri::command]
fn secret_set(key: String, value: String) -> Result<(), String> {
    secrets::set(&key, &value)
}

#[tauri::command]
fn secret_clear(key: String) -> Result<(), String> {
    secrets::clear(&key)
}

/// Opens the configured n8n instance — the URL lives in the Credential Manager.
#[tauri::command]
fn open_n8n() {
    if let Some(url) = secrets::get("n8n-url") {
        open_url(url);
    }
}

/// Refresh buttons in the integration cards.
#[tauri::command]
async fn refresh_integration(app: AppHandle, id: String) {
    integrations::poll_once(app, &id).await;
}

/// Lets the island write to the same log as the Rust side.
#[tauri::command]
fn log_line(message: String) {
    log::line(format!("ui  {message}"));
}

// ── Settings window ───────────────────────────────────────────────────────────

/// WebView2 allows exactly one browser environment per app, and its options are
/// fixed by whichever webview is created first. Every window must therefore ask
/// for the *same* arguments as the island (see `additionalBrowserArgs` in
/// tauri.conf.json) — a mismatch makes the second window come up blank, with no
/// error anywhere.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";

/// In a dev build the pages are served by Vite, so the second window needs the
/// absolute dev URL; a bundled build resolves it inside the app bundle.
fn settings_page_url(app: &AppHandle) -> WebviewUrl {
    #[cfg(dev)]
    if let Some(mut base) = app.config().build.dev_url.clone() {
        base.set_path("/settings.html");
        return WebviewUrl::External(base);
    }
    let _ = app;
    WebviewUrl::App("settings.html".into())
}

/// The settings window is created hidden at launch and only ever shown and
/// hidden afterwards. A WebView2 window created later — on the main thread or
/// not — silently comes up blank in this app, so the window that works is the
/// one that exists before the island's webview does.
fn create_settings_window(app: &AppHandle) {
    let url = settings_page_url(app);
    match WebviewWindowBuilder::new(app, "settings", url)
        .additional_browser_args(BROWSER_ARGS)
        .title("Settings — Coucou")
        .inner_size(560.0, 680.0)
        .min_inner_size(460.0, 480.0)
        .resizable(true)
        .visible(false)
        .center()
        .build()
    {
        Ok(win) => {
            // Closing it must only hide it, or it could never be reopened.
            let hidden = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden.hide();
                }
            });
        }
        Err(err) => log::line(format!("settings window failed: {err}")),
    }
}

pub fn show_settings_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("settings") else {
        log::line("settings window missing");
        return;
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    show_settings_window(&app);
}

pub fn run() {
    let loaded = settings::load();
    let gate = Arc::new(PollGate::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = app.emit_to(island::WINDOW_LABEL, "tray", "open".to_string());
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .manage(Shared {
            settings: Mutex::new(loaded.clone()),
            gate: gate.clone(),
        })
        .manage(Pending::default())
        // Opened here so a broken database is a startup failure the user hears
        // about, not a failure on their first question. A store that will not
        // open leaves the app running with chat disabled rather than not at all.
        .manage(
            ChatStore::open()
                .unwrap_or_else(|e| {
                    crate::log::line(format!("sessao salva indisponivel: {e}"));
                    ChatStore::open_at(std::path::Path::new(":memory:"))
                        .unwrap_or_else(|_| panic!("nem em memoria"))
                }),
        )
        // Which conversation the island is in; None until the first send.
        .manage(Session::default())
        .invoke_handler(tauri::generate_handler![
            boot,
            save_settings,
            set_shell_settings,
            set_collapsed,
            set_island_rect,
            focus_window,
            reposition,
            open_url,
            open_in_vscode,
            quit_app,
            hooks_status,
            hooks_preview,
            hooks_apply,
            approval_decision,
            approval_ack,
            approval_decline,
            log_line,
            chat_send,
            chat_reset,
    chat_sessions,
    chat_search,
            ollama_models,
            ingest_file,
            secret_present,
            secret_set,
            secret_clear,
            refresh_integration,
            open_n8n,
            open_settings_window,
            set_paused,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::build(&handle)?;
            // Before the island: see create_settings_window.
            create_settings_window(&handle);

            if let Some(win) = island::window(&handle) {
                island::make_non_activating(&win);
                island::apply_geometry(&handle, &loaded.screen, false);
                let _ = win.show();
            }
            gate.collapsed.store(false, Ordering::Relaxed);
            gate.set_active(true);
            island::spawn_cursor_poll(handle.clone(), gate.clone());

            log::line(format!("--- Coucou {} started ---", env!("CARGO_PKG_VERSION")));
            hooks::ensure_hook_exe(&handle);
            pipe::start(handle.clone());
            integrations::start(handle.clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Coucou");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// An Ollama daemon answering `/api/tags`, on a port the OS picked.
    fn fake_daemon(tags_body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("porta livre");
        let port = listener.local_addr().expect("endereco").port();
        let body = tags_body.to_string();
        std::thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut head = [0u8; 2048];
                let _ = stream.read(&mut head);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn an_empty_ollama_model_is_taken_from_the_daemon() {
        // The cloud model is listed first on purpose. Picking it would send the
        // chat to Ollama's own servers with no key behind it, which fails in a
        // way that looks like the chat is broken.
        let base = fake_daemon(
            r#"{"models":[{"name":"kimi-k3:cloud"},{"name":"qwen2.5:0.5b"}]}"#,
        );
        assert_eq!(
            auto_model(ollama::PROVIDER_OLLAMA, "", &base).await.as_deref(),
            Some("qwen2.5:0.5b")
        );
    }

    #[tokio::test]
    async fn a_model_the_user_chose_is_left_alone() {
        // No daemon is started: if this reached out it would fail or hang
        // instead of quietly answering None.
        assert_eq!(
            auto_model(ollama::PROVIDER_OLLAMA, "gemma4:31b-cloud", "http://127.0.0.1:1").await,
            None,
            "um modelo escolhido nao pode ser trocado"
        );
    }

    #[tokio::test]
    async fn another_provider_never_gets_an_ollama_model() {
        let base = fake_daemon(r#"{"models":[{"name":"qwen2.5:0.5b"}]}"#);
        assert_eq!(auto_model("openai", "", &base).await, None);
    }

    #[tokio::test]
    async fn nothing_local_leaves_the_model_empty_for_the_provider_to_complain() {
        let base = fake_daemon(r#"{"models":[{"name":"gemma4:31b-cloud"}]}"#);
        assert_eq!(
            auto_model(ollama::PROVIDER_OLLAMA, "  ", &base).await,
            None,
            "inventar um nome seria pior do que o erro do provider"
        );
    }

    #[tokio::test]
    async fn a_daemon_that_is_down_changes_nothing() {
        assert_eq!(auto_model(ollama::PROVIDER_OLLAMA, "", "http://127.0.0.1:1").await, None);
    }

    #[tokio::test]
    async fn the_auto_chosen_model_reaches_both_the_app_and_the_file() {
        let dir = std::env::temp_dir().join(format!("coucou-remember-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = fake_daemon(r#"{"models":[{"name":"qwen2.5:0.5b"}]}"#);

        let mut s = settings::with_all_agents(settings::Settings {
            chat_provider: ollama::PROVIDER_OLLAMA.to_string(),
            ..settings::Settings::default()
        });
        s.providers.insert(
            ollama::PROVIDER_OLLAMA.to_string(),
            settings::ProviderSettings {
                model: String::new(),
                base_url: base.clone(),
            },
        );
        let shared = Shared {
            settings: Mutex::new(s),
            gate: Arc::new(island::PollGate::new()),
        };

        // Deliberately not `remember_auto_model`: that one writes to the real
        // %APPDATA%\Coucou\settings.json, which a test has no business
        // touching. The two halves are checked instead.
        let picked = auto_model(ollama::PROVIDER_OLLAMA, "", &base).await;
        assert_eq!(picked.as_deref(), Some("qwen2.5:0.5b"));

        let snapshot = apply_auto_model(&shared, &base, picked.as_deref().unwrap());
        assert_eq!(
            shared.settings.lock().unwrap().providers[ollama::PROVIDER_OLLAMA].model,
            "qwen2.5:0.5b",
            "o app em memoria ficou sem o modelo"
        );

        // And it survives being written and read back, which is what stops the
        // next launch from starting over.
        settings::save_at(&dir, &snapshot).expect("settings gravam");
        let json = std::fs::read_to_string(dir.join("settings.json")).expect("le o arquivo");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json valido");
        assert_eq!(parsed["providers"]["ollama"]["model"], "qwen2.5:0.5b");
        assert_eq!(
            parsed["providers"]["ollama"]["baseUrl"], base,
            "o endereco do daemon foi perdido"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}