//! What the page can ask for. Errors come back as text the page shows.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::config::{LauncherConfig, exe_problem};
use crate::keymap;
use crate::launch;
use crate::options::GameOptions;
use crate::paths::{Repo, Sdk, logs_dir};
use crate::update;

#[derive(Serialize)]
pub(crate) struct Status {
    exe: Option<PathBuf>,
    exe_problem: Option<String>,
    built: bool,
    dlss_files: bool,
    fsr_files: bool,
    options: GameOptions,
    keymap: BTreeMap<String, String>,
    version: &'static str,
}

fn status_of(repo: &Repo) -> Status {
    let config = LauncherConfig::file()
        .map(|f| LauncherConfig::load(&f))
        .unwrap_or_default();
    let exe_problem = if !repo.is_checkout() {
        Some(
            "Put the launcher in your VIctory-NVIDIA folder (the one with the scripts folder)."
                .into(),
        )
    } else {
        match &config.exe {
            Some(exe) => exe_problem(exe, repo),
            None => Some("Choose the title's .exe in your own dump.".into()),
        }
    };
    Status {
        exe: config.exe,
        exe_problem,
        built: repo.built_victory().is_file(),
        dlss_files: repo.has_sdk(Sdk::Dlss),
        fsr_files: repo.has_sdk(Sdk::Fsr),
        options: GameOptions::load(&repo.menu_file()),
        keymap: keymap::load(&repo.menu_file()),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
pub(crate) fn status(repo: State<'_, Repo>) -> Status {
    status_of(&repo)
}

/// Async so the native picker does not run on the window's own thread.
#[tauri::command]
pub(crate) async fn pick_exe(
    app: tauri::AppHandle,
    repo: State<'_, Repo>,
) -> Result<Status, String> {
    let picked = app
        .dialog()
        .file()
        .set_title("The title's executable in your own dump")
        .add_filter("Executable", &["exe"])
        .blocking_pick_file();
    let Some(picked) = picked else {
        return Ok(status_of(&repo));
    };
    let exe = picked.into_path().map_err(|e| e.to_string())?;
    if let Some(problem) = exe_problem(&exe, &repo) {
        return Err(problem);
    }
    let file = LauncherConfig::file().ok_or("LOCALAPPDATA is not set.")?;
    LauncherConfig { exe: Some(exe) }
        .save(&file)
        .map_err(|e| format!("{e:#}"))?;
    Ok(status_of(&repo))
}

#[tauri::command]
pub(crate) fn save_options(options: GameOptions, repo: State<'_, Repo>) -> Result<(), String> {
    options.validate()?;
    options
        .save(&repo.menu_file())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) fn play(options: GameOptions, repo: State<'_, Repo>) -> Result<(), String> {
    save_options(options, repo.clone())?;
    let status = status_of(&repo);
    if let Some(problem) = status.exe_problem {
        return Err(problem);
    }
    let exe = status.exe.ok_or("Choose your dump first.")?;
    launch::in_new_console(launch::play_command(&repo, &exe)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) fn fetch(sdk: Sdk, repo: State<'_, Repo>) -> Result<(), String> {
    launch::in_new_console(launch::fetch_command(&repo, sdk)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) fn open_logs() -> Result<(), String> {
    let dir = logs_dir().ok_or("LOCALAPPDATA is not set.")?;
    launch::open_folder(&dir).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub(crate) fn save_keymap(
    keymap: BTreeMap<String, String>,
    repo: State<'_, Repo>,
) -> Result<(), String> {
    keymap::save(&repo.menu_file(), &keymap)
}

/// The window has no system title bar: the page's own one moves it.
#[tauri::command]
pub(crate) fn drag(window: tauri::Window) -> Result<(), String> {
    window.start_dragging().map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn minimize(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

/// Async, with git on a worker thread: fetching can take seconds.
#[tauri::command]
pub(crate) async fn check_updates(repo: State<'_, Repo>) -> Result<update::UpdateInfo, String> {
    let repo = repo.inner().clone();
    tauri::async_runtime::spawn_blocking(move || update::check(&repo))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

/// Starts the update script and closes the launcher so it can be rebuilt.
#[tauri::command]
pub(crate) async fn apply_update(
    remote: String,
    app: tauri::AppHandle,
    repo: State<'_, Repo>,
) -> Result<(), String> {
    let repo = repo.inner().clone();
    let checked = repo.clone();
    let info = tauri::async_runtime::spawn_blocking(move || update::check(&checked))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    if info.up_to_date() {
        return Err("Already up to date.".into());
    }
    if let Some(reason) = info.blocked() {
        return Err(reason.to_owned());
    }
    if remote != info.remote() {
        return Err("The repository's remotes changed: check again.".into());
    }
    let cmd = update::apply_command(&repo, &remote).map_err(|e| format!("{e:#}"))?;
    launch::in_new_console(cmd).map_err(|e| format!("{e:#}"))?;
    app.exit(0);
    Ok(())
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Link {
    Official,
    Original,
    Author,
}

/// Only these addresses: the page cannot open others.
#[tauri::command]
pub(crate) fn open_link(link: Link) -> Result<(), String> {
    let url = match link {
        Link::Official => update::OFFICIAL,
        Link::Original => update::ORIGINAL,
        Link::Author => update::AUTHOR,
    };
    launch::open_url(url).map_err(|e| format!("{e:#}"))
}

/// The music files built into the launcher, as paths the page can play.
#[tauri::command]
pub(crate) fn songs() -> Vec<String> {
    env!("LAUNCHER_SONGS")
        .split('|')
        .filter(|n| !n.is_empty())
        .map(|n| format!("assets/song/{n}"))
        .collect()
}
