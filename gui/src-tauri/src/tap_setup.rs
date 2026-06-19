//! Гарантировать наличие хотя бы одного tap-windows6 адаптера.
//!
//! Установщик драйвера ставит сам драйвер, но экземпляр адаптера может не
//! создаться (зависит от сборки инсталлятора). Backend (`tap::open`) ищет
//! готовый адаптер в реестре, поэтому после установки досоздаём его через
//! `tapctl.exe create` — но только если адаптера ещё нет (иначе плодились бы
//! дубли при повторном «Установить»).

#[cfg(windows)]
use std::path::PathBuf;

/// Убедиться, что в системе есть tap-windows6 адаптер; если нет — создать.
///
/// # Errors
/// Текст ошибки, если адаптер отсутствует и создать его не удалось (нет
/// `tapctl.exe` — драйвер не установлен; либо `tapctl` вернул ненулевой код).
#[cfg(windows)]
pub fn ensure_adapter() -> Result<(), String> {
    // Уже есть готовый адаптер — ничего не делаем.
    if lattice_client::tap::find_tap_adapter().is_some() {
        log::info!("tap adapter already present");
        return Ok(());
    }

    let tapctl = find_tapctl().ok_or_else(|| {
        "tapctl.exe не найден — TAP-драйвер не установлен".to_string()
    })?;
    log::info!("creating tap adapter via {}", tapctl.display());

    // tapctl create — по умолчанию hwid root\tap0901 (tap-windows6).
    let out = std::process::Command::new(&tapctl)
        .args(["create", "--name", "Lattice"])
        .output()
        .map_err(|e| format!("не удалось запустить tapctl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tapctl create: {}", err.trim()));
    }

    // Перепроверяем: адаптер должен появиться в реестре.
    if lattice_client::tap::find_tap_adapter().is_some() {
        Ok(())
    } else {
        Err("адаптер не появился после tapctl create".to_string())
    }
}

#[cfg(not(windows))]
pub fn ensure_adapter() -> Result<(), String> {
    Ok(())
}

/// Найти `tapctl.exe` в типичных местах установки tap-windows6 / OpenVPN.
#[cfg(windows)]
fn find_tapctl() -> Option<PathBuf> {
    let roots = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"];
    let subs = [
        "TAP-Windows\\bin\\tapctl.exe",
        "OpenVPN\\bin\\tapctl.exe",
        "OpenVPN\\tap-windows6\\tapctl.exe",
    ];
    for root in roots {
        let Ok(base) = std::env::var(root) else {
            continue;
        };
        for sub in subs {
            let cand = PathBuf::from(&base).join(sub);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
