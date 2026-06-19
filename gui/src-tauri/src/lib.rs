//! Tauri-мост Lattice: команды (`#[tauri::command]`) + события + трей.
//!
//! Контракт: фронт общается с backend ТОЛЬКО через эти команды и события.
//! Бизнес-логика (сеть/крипто) — в `lattice-client`; здесь оркестрация и UI-мост.

mod conn;
mod logbuf;
mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};

use crate::settings::Settings;

/// Разделяемое состояние приложения.
struct AppState {
    settings_path: PathBuf,
    /// Флаг shutdown активного подключения (если есть).
    conn: Mutex<Option<Arc<AtomicBool>>>,
}

// --- Команды ---------------------------------------------------------------

/// Подключиться: название+пароль уходят в backend как есть, KDF→ключ→network-id
/// считает Rust (см. `conn::derive_key`). Возврат — мгновенный; статус и список
/// пиров придут событиями.
#[tauri::command]
fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    network: String,
    password: String,
) -> Result<(), String> {
    if network.trim().is_empty() || password.is_empty() {
        return Err("empty network or password".into());
    }
    let settings = Settings::load(&state.settings_path);
    let mut guard = state.conn.lock().map_err(|e| e.to_string())?;
    if let Some(old) = guard.take() {
        old.store(true, Ordering::Release); // остановить предыдущее подключение.
    }
    *guard = Some(conn::spawn(app, settings, network, password));
    Ok(())
}

/// Отключиться: выставить флаг shutdown активному потоку.
#[tauri::command]
fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(flag) = state.conn.lock().map_err(|e| e.to_string())?.take() {
        flag.store(true, Ordering::Release);
    }
    Ok(())
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    Settings::load(&state.settings_path)
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    settings.save(&state.settings_path)
}

/// Снимок последних строк лога — для кнопки «Скопировать лог» в Диагностике.
#[tauri::command]
fn copy_log() -> String {
    logbuf::snapshot()
}

// --- Трей ------------------------------------------------------------------

fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Открыть Lattice", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;

    let mut builder = TrayIconBuilder::with_id("lattice-tray")
        .tooltip("Lattice")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Левый клик по иконке — показать окно.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logbuf::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let path = app
                .path()
                .app_config_dir()
                .map(|d| d.join("settings.json"))
                .unwrap_or_else(|_| PathBuf::from("lattice-settings.json"));
            app.manage(AppState {
                settings_path: path,
                conn: Mutex::new(None),
            });
            build_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Закрытие окна не выходит из приложения — сворачиваем в трей.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            get_settings,
            save_settings,
            copy_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
