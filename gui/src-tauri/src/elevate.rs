//! Самоповышение прав. Виртуальный адаптер и `netsh` требуют администратора;
//! чтобы пользователь не делал «Запустить от имени администратора» вручную,
//! приложение при старте проверяет токен и, если прав нет, перезапускает себя
//! через UAC (`ShellExecute` с глаголом `runas`), а текущий процесс закрывает.
//!
//! Делается ДО создания окна Tauri — так UAC-промпт появляется один раз, до
//! WebView, и дальше всё работает без сюрпризов «нет прав» в середине сценария.

/// Убедиться, что процесс запущен с правами администратора. `true` — можно
/// продолжать; `false` — был запущен перезапуск с повышением, текущий экземпляр
/// должен немедленно выйти.
#[cfg(windows)]
#[must_use]
pub fn ensure_admin() -> bool {
    // check_admin() из backend = Win32 CheckTokenMembership (весь Win32 по
    // контракту живёт в lattice-client::tap). Ok → уже elevated.
    if lattice_client::netcfg::check_admin().is_ok() {
        return true;
    }
    if let Err(e) = relaunch_elevated() {
        // Не смогли перезапустить (пользователь отклонил UAC и т.п.) — пусть
        // приложение запустится без прав: при «Подключиться» покажется внятная
        // ошибка not_admin (старый путь как fallback).
        log::warn!("relaunch elevated failed: {e}; continuing unelevated");
        return true;
    }
    false
}

#[cfg(not(windows))]
#[must_use]
pub fn ensure_admin() -> bool {
    true
}

#[cfg(windows)]
fn relaunch_elevated() -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args: String = std::env::args()
        .skip(1)
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");

    let to_wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    };
    let verb = to_wide(std::ffi::OsStr::new("runas"));
    let file = to_wide(exe.as_os_str());
    let params = to_wide(std::ffi::OsStr::new(&args));

    // ShellExecuteW возвращает HINSTANCE; значение > 32 = успех.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            if args.is_empty() {
                std::ptr::null()
            } else {
                params.as_ptr()
            },
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (rc as isize) > 32 {
        log::info!("relaunched elevated; exiting unelevated instance");
        Ok(())
    } else {
        Err(format!("ShellExecuteW returned {}", rc as isize))
    }
}
