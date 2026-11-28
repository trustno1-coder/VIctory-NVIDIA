//! A window to pick your own dump, choose the options the F1 menu offers and
//! start the game through `scripts\play-nvidia.ps1`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod keymap;
mod launch;
mod options;
mod paths;
mod settings_file;
mod update;

fn main() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(paths::Repo::of_this_build())
        .invoke_handler(tauri::generate_handler![
            commands::status,
            commands::pick_exe,
            commands::save_options,
            commands::play,
            commands::fetch,
            commands::open_logs,
            commands::quit,
            commands::save_keymap,
            commands::drag,
            commands::minimize,
            commands::check_updates,
            commands::apply_update,
            commands::open_link,
            commands::songs,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        show_error(&format!(
            "The launcher's window could not open: {e}\n\n\
             It needs Microsoft Edge WebView2, which Windows 11 has. On Windows 10, \
             install the WebView2 Runtime from Microsoft's site, then start the launcher again."
        ));
    }
}

/// A plain message box: the web page may be what failed.
fn show_error(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (text, title) = (wide(text), wide("VIctory Launcher"));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}
