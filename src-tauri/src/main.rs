// Walk Relay desktop app: a tray icon that keeps Big Walk saves in sync.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use walkrelay_core::engine::{self, Status, Version};
use walkrelay_core::{
    config, paths, schedule, AppConfig, Engine, EngineOptions, Event, GameDetector, GroupCode, Invite, Member, R2Store,
    WorkerAuth, WorkerClient,
};

/// First update check after launch, then this often.
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(30);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often to check whether Big Walk has started or stopped.
const GAME_CHECK_EVERY: Duration = Duration::from_secs(4);
/// After Big Walk closes, wait this long for its final save, then sync.
const AFTER_EXIT_DELAY: Duration = Duration::from_secs(7);

enum Wake {
    SyncNow,
    Reload,
    Quit,
}

struct Shared {
    data_dir: PathBuf,
    config: Mutex<Option<AppConfig>>,
    status: Mutex<Status>,
    wake: Sender<Wake>,
    tray_status: Mutex<Option<MenuItem<Wry>>>,
    /// Group code from an invite link the UI hasn't picked up yet.
    pending_invite: Mutex<Option<String>>,
    update: Mutex<UpdateState>,
    /// Set while installing an update: the worker stops syncing.
    updating: AtomicBool,
    /// True while the worker is in the middle of a sync.
    syncing: AtomicBool,
}

#[derive(Serialize, Clone, Default)]
struct UpdateState {
    /// Newer version on GitHub, if any.
    available: Option<String>,
    notes: Option<String>,
    checked_at: Option<u64>,
    error: Option<String>,
    /// Version we've already sent a notification about.
    #[serde(skip)]
    notified: Option<String>,
}

type St<'a> = tauri::State<'a, Arc<Shared>>;

// ------------------------------------------------------------------ commands

#[derive(Serialize)]
struct UiState {
    configured: bool,
    player_name: Option<String>,
    status: Status,
    candidate_dirs: Vec<String>,
    app_version: String,
    update: UpdateState,
    /// Secured group: members use personal passes via the group's Worker.
    secured: bool,
    worker_url: Option<String>,
}

#[tauri::command]
fn get_state(app: AppHandle, shared: St<'_>) -> UiState {
    let cfg = shared.config.lock().unwrap().clone();
    UiState {
        configured: cfg.is_some(),
        player_name: cfg.as_ref().map(|c| c.player_name.clone()),
        status: shared.status.lock().unwrap().clone(),
        candidate_dirs: paths::candidate_save_dirs().iter().map(|p| p.display().to_string()).collect(),
        app_version: app.package_info().version.to_string(),
        update: shared.update.lock().unwrap().clone(),
        secured: cfg.as_ref().is_some_and(|c| c.is_secured()),
        worker_url: cfg.as_ref().and_then(|c| c.worker.as_ref().map(|w| w.url.clone())),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn join_group(app: AppHandle, shared: St<'_>, code: String, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 32 {
        return Err("Pick a name between 1 and 32 characters, so friends know who's hosting.".into());
    }
    // Accept a pasted invite link or message, not only the bare code.
    let cfg = match config::parse_invite(&code) {
        Some(Invite::Worker { url, id }) => {
            let n = name.clone();
            let u = url.clone();
            let (token, _) = blocking(move || WorkerClient::redeem(&u, &id, &n)).await?;
            AppConfig {
                group: None,
                worker: Some(WorkerAuth { url, token, state_id: String::new() }),
                player_name: name,
                extra_save_dirs: Vec::new(),
            }
        }
        other => {
            let code = match other {
                Some(Invite::Direct(c)) => c,
                _ => code,
            };
            let group = GroupCode::decode(&code).map_err(|e| format!("{e:#}"))?;
            let g2 = group.clone();
            blocking(move || R2Store::new(&g2)?.check_access())
                .await
                .map_err(|e| format!("Couldn't reach your group's storage: {e}"))?;
            AppConfig { group: Some(group), worker: None, player_name: name, extra_save_dirs: Vec::new() }
        }
    };
    cfg.save(&shared.data_dir).map_err(|e| format!("{e:#}"))?;
    *shared.config.lock().unwrap() = Some(cfg);
    if let Err(e) = app.autolaunch().enable() {
        log::warn!("autostart: {e}");
    }
    let _ = shared.wake.send(Wake::Reload);
    Ok(())
}

#[tauri::command]
async fn create_group_code(endpoint: String, bucket: String, key_id: String, secret: String) -> Result<String, String> {
    let g = GroupCode {
        endpoint: endpoint.trim().trim_end_matches('/').to_string(),
        bucket: bucket.trim().to_string(),
        key_id: key_id.trim().to_string(),
        secret: secret.trim().to_string(),
        region: "auto".into(),
    };
    g.validate().map_err(|e| format!("{e:#}"))?;
    let g2 = g.clone();
    blocking(move || R2Store::new(&g2)?.check_access())
        .await
        .map_err(|e| format!("Those details didn't work: {e}"))?;
    Ok(g.encode())
}

/// The shareable code for an older (unsecured) group. Secured groups have none.
#[tauri::command]
fn get_group_code(shared: St<'_>) -> Option<String> {
    shared.config.lock().unwrap().as_ref().and_then(|c| c.group.as_ref()).map(|g| g.encode())
}

/// An invite to send: for a secured group, a fresh single-use link from the
/// Worker; otherwise the group code's link (or the given code's).
async fn make_invite(shared: &Arc<Shared>, code: Option<String>) -> Result<(String, bool), String> {
    if let Some(code) = code {
        return Ok((config::invite_link(&compact_code(&code)?), false));
    }
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    if let Some(w) = cfg.worker {
        let inv = blocking(move || WorkerClient::new(&w.url, &w.token)?.create_invite()).await?;
        return Ok((inv.url, true));
    }
    let g = cfg.group.ok_or("Not set up yet")?;
    Ok((config::invite_link(&g.encode()), false))
}

/// The friendly message to send friends, with the clickable invite link.
#[tauri::command]
async fn invite_message(shared: St<'_>, code: Option<String>) -> Result<String, String> {
    let (link, single_use) = make_invite(shared.inner(), code).await?;
    Ok(if single_use {
        config::discord_message_for_link(&link)
    } else {
        config::discord_invite_message(&config::code_from_invite(&link).unwrap_or_default())
    })
}

/// Just the invite link, for apps without Discord-style link text.
#[tauri::command]
async fn invite_link(shared: St<'_>, code: Option<String>) -> Result<String, String> {
    Ok(make_invite(shared.inner(), code).await?.0)
}

/// Upgrade an older group: this computer becomes the owner of the group's
/// Worker, and stops holding the bucket key.
#[tauri::command]
async fn secure_group(shared: St<'_>, url: String) -> Result<(), String> {
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    let group = cfg.group.clone().ok_or("This group is already secured.")?;
    let url = walkrelay_core::worker::normalize_url(&url).map_err(|e| format!("{e:#}"))?;
    let (u, n, g) = (url.clone(), cfg.player_name.clone(), group.clone());
    let (token, _) = blocking(move || WorkerClient::setup(&u, &R2Store::new(&g)?, &n)).await?;
    let secured = AppConfig {
        group: None,
        worker: Some(WorkerAuth { url, token, state_id: group.id() }),
        ..cfg
    };
    secured.save(&shared.data_dir).map_err(|e| format!("{e:#}"))?;
    *shared.config.lock().unwrap() = Some(secured);
    let _ = shared.wake.send(Wake::Reload);
    Ok(())
}

#[derive(Serialize)]
struct MembersView {
    me: Member,
    members: Vec<Member>,
}

fn worker_client(shared: &Arc<Shared>) -> Result<WorkerClient, String> {
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    let w = cfg.worker.ok_or("Members are only available in a secured group.")?;
    WorkerClient::new(&w.url, &w.token).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_members(shared: St<'_>) -> Result<MembersView, String> {
    let c = worker_client(shared.inner())?;
    blocking(move || Ok(MembersView { me: c.me()?, members: c.members()? })).await
}

#[tauri::command]
async fn remove_member(shared: St<'_>, id: String) -> Result<(), String> {
    let c = worker_client(shared.inner())?;
    blocking(move || c.remove_member(&id)).await
}

/// Re-encode any code (old or new format) in its shortest form.
fn compact_code(code: &str) -> Result<String, String> {
    let code = config::code_from_invite(code).ok_or("That group code doesn't look right")?;
    Ok(GroupCode::decode(&code).map_err(|e| format!("{e:#}"))?.encode())
}

/// A group code from an invite link that opened the app, if the UI hasn't taken it yet.
#[tauri::command]
fn take_pending_invite(shared: St<'_>) -> Option<String> {
    shared.pending_invite.lock().unwrap().take()
}

#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<UpdateState, String> {
    check_update(&app, false).await;
    let shared = app.state::<Arc<Shared>>();
    let st = shared.update.lock().unwrap().clone();
    match &st.error {
        Some(e) => Err(e.clone()),
        None => Ok(st),
    }
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let shared = app.state::<Arc<Shared>>().inner().clone();
    if GameDetector::new().is_running() {
        return Err("Close Big Walk first. Walk Relay never updates while you're playing.".into());
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("Couldn't check for updates: {e}"))?
        .ok_or("You're already on the latest version.")?;
    // Let any sync in progress finish, and don't start another.
    shared.updating.store(true, Ordering::SeqCst);
    while shared.syncing.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let result = update.download_and_install(|_, _| {}, || {}).await;
    if let Err(e) = result {
        shared.updating.store(false, Ordering::SeqCst);
        return Err(format!("The update didn't install: {e}"));
    }
    app.restart();
}

#[tauri::command]
async fn list_versions(shared: St<'_>, stem: String) -> Result<Vec<Version>, String> {
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    blocking(move || engine::list_versions(cfg.store()?.as_ref(), &stem)).await
}

#[derive(Serialize)]
struct DeletedJourney {
    stem: String,
    name: String,
}

#[tauri::command]
async fn list_deleted(shared: St<'_>) -> Result<Vec<DeletedJourney>, String> {
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    let list = blocking(move || engine::list_deleted(cfg.store()?.as_ref())).await?;
    Ok(list.into_iter().map(|(stem, name)| DeletedJourney { stem, name }).collect())
}

#[tauri::command]
async fn restore_version(shared: St<'_>, stem: String, key: String) -> Result<(), String> {
    let cfg = shared.config.lock().unwrap().clone().ok_or("Not set up yet")?;
    let player = cfg.player_name.clone();
    blocking(move || engine::restore_version(cfg.store()?.as_ref(), &stem, &key, &player)).await?;
    let _ = shared.wake.send(Wake::SyncNow);
    Ok(())
}

#[tauri::command]
fn sync_now(shared: St<'_>) {
    let _ = shared.wake.send(Wake::SyncNow);
}

#[tauri::command]
fn open_save_folder(app: AppHandle, shared: St<'_>) -> Result<(), String> {
    let extra = shared.config.lock().unwrap().as_ref().map(|c| c.extra_save_dirs.clone()).unwrap_or_default();
    let dir = paths::existing_save_dirs(&extra)
        .into_iter()
        .next()
        .ok_or("Big Walk's save folder doesn't exist yet. Launch Big Walk once, then try again.")?;
    app.opener().open_path(dir.display().to_string(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_backups_folder(app: AppHandle, shared: St<'_>) -> Result<(), String> {
    let dir = shared.data_dir.join("backups");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    app.opener().open_path(dir.display().to_string(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn leave_group(shared: St<'_>) -> Result<(), String> {
    let _ = std::fs::remove_file(AppConfig::path(&shared.data_dir));
    *shared.config.lock().unwrap() = None;
    let _ = shared.wake.send(Wake::Reload);
    Ok(())
}

// ------------------------------------------------------------------ worker

fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("notification failed: {e}");
    }
}

fn announce(app: &AppHandle, events: &[Event], game_running: bool) {
    for e in events {
        match e {
            Event::Uploaded { journey } if !game_running => {
                notify(app, "Journey shared", &format!("“{journey}” is synced. Anyone can host it next."))
            }
            Event::Uploaded { .. } => {}
            Event::Downloaded { journey, from } => {
                notify(app, "Journey updated", &format!("Got the latest “{journey}” from {from}. You're ready to host."))
            }
            Event::Conflict { journey, kept_from, saved_copy_from } => notify(
                app,
                "Two versions of a journey",
                &format!(
                    "{kept_from} and {saved_copy_from} both played “{journey}”. Kept {kept_from}'s newer one; the other is saved under Versions in Walk Relay."
                ),
            ),
            Event::DeletedLocal { journey } | Event::DeletedRemote { journey } => notify(
                app,
                "Journey deleted",
                &format!("“{journey}” was deleted. You can bring it back from Settings → Deleted journeys in Walk Relay."),
            ),
            Event::PeerStartedHosting { player, journey } => {
                notify(app, &format!("{player} is hosting"), &format!("{player} is hosting “{journey}”. Hop in!"))
            }
            Event::PeerAlreadyHosting { player, journey } => notify(
                app,
                &format!("{player} is already hosting"),
                &format!("{player} is hosting “{journey}” right now. Join them rather than hosting it too, or the journey will split in two."),
            ),
            Event::NearFreeLimit { percent } => notify(
                app,
                "Cloudflare free allowance",
                &format!(
                    "Your group has used about {percent}% of Cloudflare R2's free monthly allowance. Syncing carries on; going over costs a few cents."
                ),
            ),
        }
    }
}

fn tray_line(status: &Status, configured: bool) -> String {
    if !configured {
        return "Not set up yet: click to open".into();
    }
    if let Some(h) = &status.hosting {
        return format!("You're hosting “{h}”");
    }
    if let Some(p) = status.peers.iter().find(|p| p.hosting.is_some()) {
        return format!("{} is hosting “{}”", p.player, p.hosting.as_deref().unwrap_or_default());
    }
    if status.last_error.is_some() {
        return "Can't sync right now; retrying".into();
    }
    if status.save_dirs.is_empty() && status.last_sync_at.is_some() {
        return "Big Walk save folder not found yet".into();
    }
    if status.last_sync_at.is_none() {
        return "Starting…".into();
    }
    if status.journeys.iter().all(|j| j.in_sync) {
        "✓ All journeys in sync".into()
    } else {
        "Syncing…".into()
    }
}

fn worker(app: AppHandle, shared: Arc<Shared>, rx: Receiver<Wake>) {
    let mut engine: Option<Engine> = None;
    let mut detector = GameDetector::new();
    let mut prev_running = detector.is_running();
    let mut last_exit: Option<Instant> = None;
    let mut next_sync = Instant::now();
    let mut last_tray = String::new();

    loop {
        // (Re)build the engine from config.
        if engine.is_none() {
            if let Some(cfg) = shared.config.lock().unwrap().clone() {
                let mut opts = EngineOptions::new(cfg.player_name.clone(), shared.data_dir.clone());
                opts.extra_save_dirs = cfg.extra_save_dirs.clone();
                opts.state_file = format!("state-{}.json", cfg.state_id());
                opts.usage_file = format!("usage-{}.json", cfg.state_id());
                match cfg.store().and_then(|s| Engine::new(s, opts)) {
                    Ok(e) => engine = Some(e),
                    Err(e) => shared.status.lock().unwrap().last_error = Some(format!("{e:#}")),
                }
            }
        }

        let running = detector.is_running();
        if running && !prev_running {
            next_sync = Instant::now(); // publish "hosting/playing" quickly and warn about clashes
        }
        if !running && prev_running {
            next_sync = Instant::now() + AFTER_EXIT_DELAY;
            last_exit = Some(Instant::now());
        }
        prev_running = running;

        if Instant::now() >= next_sync && !shared.updating.load(Ordering::SeqCst) {
            shared.syncing.store(true, Ordering::SeqCst);
            if let Some(e) = engine.as_mut() {
                match e.sync_once(running) {
                    Ok(events) => announce(&app, &events, running),
                    Err(err) => log::warn!("sync failed: {err:#}"),
                }
                *shared.status.lock().unwrap() = e.status().clone();
            }
            shared.syncing.store(false, Ordering::SeqCst);
            next_sync = Instant::now() + {
                let st = shared.status.lock().unwrap();
                let friend_playing = !st.peers.is_empty();
                let all_in_sync = st.journeys.iter().filter(|j| j.on_this_computer).all(|j| j.in_sync);
                // After a failed sync, retry at the active pace.
                let ok = st.last_error.is_none();
                schedule::sync_interval(running, friend_playing, last_exit.map(|t| t.elapsed()), all_in_sync && ok)
            };
            // A save written just before exit may still have been settling; check again soon.
            if !running && engine.as_ref().map_or(false, |e| e.status().journeys.iter().any(|j| j.on_this_computer && !j.in_sync)) {
                next_sync = next_sync.min(Instant::now() + Duration::from_secs(8));
            }
        }

        let line = tray_line(&shared.status.lock().unwrap(), engine.is_some());
        if line != last_tray {
            if let Some(item) = shared.tray_status.lock().unwrap().as_ref() {
                let _ = item.set_text(&line);
            }
            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(format!("Walk Relay: {line}")));
            }
            last_tray = line;
        }

        match rx.recv_timeout(GAME_CHECK_EVERY) {
            Ok(Wake::SyncNow) => next_sync = Instant::now(),
            Ok(Wake::Reload) => {
                if let Some(e) = engine.as_mut() {
                    let _ = e.clear_presence();
                }
                engine = None;
                *shared.status.lock().unwrap() = Status::default();
                next_sync = Instant::now();
            }
            Ok(Wake::Quit) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(e) = engine.as_mut() {
                    let _ = e.clear_presence();
                }
                app.exit(0);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

// ------------------------------------------------------------------ app

fn show_main(app: &AppHandle) {
    // Opening the window is a good moment to refresh.
    if let Some(shared) = app.try_state::<Arc<Shared>>() {
        let _ = shared.wake.send(Wake::SyncNow);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Handle an invite link (from a click, or from the command line).
fn handle_links<I: IntoIterator<Item = String>>(app: &AppHandle, links: I) {
    for link in links {
        if config::parse_invite(&link).is_some() {
            *app.state::<Arc<Shared>>().pending_invite.lock().unwrap() = Some(link.clone());
            show_main(app);
            let _ = app.emit("invite", link);
        }
    }
}

/// Look for a newer release; notify once per new version.
async fn check_update(app: &AppHandle, notify_if_new: bool) {
    let shared = app.state::<Arc<Shared>>().inner().clone();
    let result = match app.updater() {
        Ok(u) => u.check().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    let mut st = shared.update.lock().unwrap();
    st.checked_at = Some(walkrelay_core::now_ms());
    match result {
        Ok(Some(u)) => {
            st.error = None;
            st.available = Some(u.version.clone());
            st.notes = u.body.clone();
            if notify_if_new && st.notified.as_deref() != Some(u.version.as_str()) {
                st.notified = Some(u.version.clone());
                notify(app, "Walk Relay update", &format!("Version {} is available. Open Walk Relay to update.", u.version));
            }
        }
        Ok(None) => {
            st.error = None;
            st.available = None;
            st.notes = None;
        }
        Err(e) => {
            log::warn!("update check failed: {e}");
            st.error = Some(format!("Couldn't check for updates: {e}"));
        }
    }
}

fn main() {
    let data_dir = config::default_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let cfg = AppConfig::load(&data_dir).unwrap_or(None);
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        data_dir,
        config: Mutex::new(cfg),
        status: Mutex::new(Status::default()),
        wake: tx,
        tray_status: Mutex::new(None),
        pending_invite: Mutex::new(None),
        update: Mutex::new(UpdateState::default()),
        updating: AtomicBool::new(false),
        syncing: AtomicBool::new(false),
    });
    let started_hidden = std::env::args().any(|a| a == "--minimized");

    tauri::Builder::default()
        // Must be first: a second launch (e.g. clicking an invite link on Windows)
        // hands its arguments to the running copy and exits.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            show_main(app);
            handle_links(app, args);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![
            get_state,
            join_group,
            create_group_code,
            get_group_code,
            invite_message,
            invite_link,
            secure_group,
            list_members,
            remove_member,
            take_pending_invite,
            check_for_update,
            install_update,
            list_versions,
            list_deleted,
            restore_version,
            sync_now,
            open_save_folder,
            open_backups_folder,
            leave_group
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let status_item = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "Open Walk Relay", true, None::<&str>)?;
            let sync = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
            let folder = MenuItem::with_id(app, "folder", "Open Big Walk save folder", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Walk Relay", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&status_item, &sep1, &open, &sync, &folder, &sep2, &quit])?;
            *shared.tray_status.lock().unwrap() = Some(status_item);

            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Walk Relay")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    let shared = app.state::<Arc<Shared>>();
                    match event.id.as_ref() {
                        "open" => show_main(app),
                        "sync" => {
                            let _ = shared.wake.send(Wake::SyncNow);
                        }
                        "folder" => {
                            if let Err(e) = open_save_folder(app.clone(), shared) {
                                notify(app, "Walk Relay", &e);
                            }
                        }
                        "quit" => {
                            let _ = shared.wake.send(Wake::Quit);
                            // Don't wait forever on a stalled network.
                            let h = app.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_secs(5));
                                h.exit(0);
                            });
                        }
                        _ => {}
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            let handle = app.handle().clone();
            let shared2 = shared.clone();
            std::thread::Builder::new()
                .name("walkrelay-sync".into())
                .spawn(move || worker(handle, shared2, rx))?;

            // Invite links: clicked while running (macOS), or the one that launched us.
            let h = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                handle_links(&h, event.urls().into_iter().map(|u| u.to_string()));
            });
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                handle_links(app.handle(), urls.into_iter().map(|u| u.to_string()));
            }
            handle_links(app.handle(), std::env::args().skip(1));

            // Check for updates shortly after launch, then daily.
            let h = app.handle().clone();
            std::thread::Builder::new().name("walkrelay-update".into()).spawn(move || {
                std::thread::sleep(FIRST_UPDATE_CHECK);
                loop {
                    tauri::async_runtime::block_on(check_update(&h, true));
                    std::thread::sleep(UPDATE_CHECK_EVERY);
                }
            })?;

            let configured = shared.config.lock().unwrap().is_some();
            if !configured || !started_hidden {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window just hides it; Walk Relay keeps syncing from the tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while starting Walk Relay")
        .run(|_app, _event| {
            // Opening the app again from Finder/Launchpad shows the window.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                show_main(_app);
            }
        });
}
