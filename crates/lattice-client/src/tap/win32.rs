//! Общие Win32-помощники, не относящиеся к TAP-драйверу напрямую, но
//! требующие `unsafe` Win32-вызовов. Сосредоточены здесь, в модуле `tap`,
//! чтобы соблюсти контракт AGENTS.md: «весь `unsafe` / Win32 — только в
//! `lattice-client/src/tap/`». Сюда входят:
//!
//! - `check_admin` — проверка прав администратора через `CheckTokenMembership`
//!   (Win32 Security API). Вызывается из `netcfg` перед поднятием линка.
//! - `install_ctrl_handler` — регистрация `SetConsoleCtrlHandler` для
//!   graceful shutdown по Ctrl+C / закрытию консоли. Вызывается из `main`.

#![cfg_attr(not(windows), allow(unused))]

use super::TapError;

/// Проверить, что процесс запущен с правами администратора. Способ —
/// `CheckTokenMembership` против SID builtin-administrators
/// (`WinBuiltinAdministratorsSid`). Возвращает `Err(TapError::NotElevated)`,
/// если токен не в группе — вызывающий (`netcfg`) мапит это в `NetcfgError`.
///
/// # Errors
///
/// `NotElevated` — токен не в группе Administrators; `Registry` — Win32-ошибка
/// при работе с SID/токеном (diagnosed message).
#[cfg(windows)]
pub fn check_admin() -> Result<(), TapError> {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID,
        SID_IDENTIFIER_AUTHORITY,
    };

    // SECURITY_NT_AUTHORITY = {0,0,0,0,0,5}. Под этим authority лежат
    // builtin-группы, включая Administrators.
    let authority = SID_IDENTIFIER_AUTHORITY { Value: [0, 0, 0, 0, 0, 5] };
    let mut admin_sid: PSID = std::ptr::null_mut();

    // SAFETY: AllocateAndInitializeSid записывает в `admin_sid` указатель на
    // выделенный SID; валиден до FreeSid. nSubAuthorityCount=2 соответствует
    // числу переданных sub-authority (SECURITY_BUILTIN_DOMAIN_RID=0x20,
    // DOMAIN_ALIAS_RID_ADMINS=0x220); остальные 6 нулей функция не читает.
    unsafe {
        let ok = AllocateAndInitializeSid(
            &authority,
            2,
            0x20,
            0x220,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admin_sid,
        );
        if ok == 0 {
            return Err(TapError::Registry(
                "AllocateAndInitializeSid failed (cannot build admin SID)".into(),
            ));
        }

        let mut is_member: BOOL = 0;
        // TokenHandle=NULL — проверить токен вызывающего потока/процесса.
        let check_ok = CheckTokenMembership(std::ptr::null_mut(), admin_sid, &mut is_member);
        // FreeSid безопасно вызывать всегда после успешного Allocate.
        FreeSid(admin_sid);

        if check_ok == 0 {
            return Err(TapError::Registry(
                "CheckTokenMembership failed (cannot determine elevation)".into(),
            ));
        }
        if is_member == 0 {
            return Err(TapError::NotElevated);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn check_admin() -> Result<(), TapError> {
    Err(TapError::Registry(
        "admin check is Windows-only (this build is non-Windows)".into(),
    ))
}

/// Тип callback для `SetConsoleCtrlHandler`. Вынесен отдельно чтобы
/// `install_ctrl_handler` мог принять любую `extern "system" fn(u32) -> i32`.
pub type CtrlHandler = extern "system" fn(u32) -> i32;

/// Зарегистрировать console-control-handler. Возвращает `false` если Win32
/// вызов провалился — вызывающий логирует warning и работает без Ctrl+C
/// graceful shutdown (процесс всё равно можно убить, но линк опустится
/// драйвером при `CloseHandle`).
#[cfg(windows)]
pub fn install_ctrl_handler(handler: CtrlHandler) -> bool {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: передаём указатель на статическую `extern "system" fn` — она
    // валидна всё время жизни процесса. Возвращаемое BOOL: 0 = неудача.
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) != 0 }
}

#[cfg(not(windows))]
pub fn install_ctrl_handler(_handler: CtrlHandler) -> bool {
    false
}
