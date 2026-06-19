//! Скан реестра на tap-windows6 адаптер. Вынесен из `tap/mod.rs` чтобы
//! разделить ответственность: lifecycle устройства и discovery — разные
//! задачи, и файл `mod.rs` не превышал лимит строк.
//!
//! Ищем в `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e972-...}` сабкеи
//! с `ComponentId = tap0901`, читаем `NetCfgInstanceId` (GUID для
//! `\\.\Global\{GUID}.tap`) и `Name` (connection name для `netsh`).

#![cfg_attr(not(windows), allow(unused))]

use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::OsStrExt;

use super::TapError;

#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_SZ,
};

/// Ключ реестра с сетевыми адаптерами. `NetCfgInstanceId` (`REG_SZ`) = GUID
/// адаптера для `\\.\Global\{GUID}.tap`; `Name` (`REG_SZ`) = connection name,
/// который понимает `netsh`. `ComponentId` (`REG_SZ`) = `tap0901` для `tap-windows6`.
#[cfg(windows)]
pub(crate) const NET_CLASS_KEY: &str =
    "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e972-e325-11ce-bfc4-08002be10318}";

/// `ComponentId` `tap-windows6`. OpenVPN-драйвер этой ветки регистрируется
/// именно так; альтернативные сборки иногда тоже объявляют `tap0901`.
pub(crate) const TAP_COMPONENT_ID: &str = "tap0901";

/// Найденный в реестре TAP-адаптер: GUID для `\\.\Global\{GUID}.tap` и имя
/// подключения для `netsh`.
#[derive(Clone)]
pub struct TapAdapterInfo {
    /// GUID в виде `{xxxxxxxx-xxxx-...}` — вставляется в `\\.\Global\{GUID}.tap`.
    pub guid: String,
    /// Connection name (registry `Name`), используется в `netsh`.
    pub name: String,
}

impl fmt::Debug for TapAdapterInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapAdapterInfo")
            .field("guid", &self.guid)
            .field("name", &self.name)
            .finish()
    }
}

/// Найти `tap-windows6` адаптер в реестре. Возвращает первый подходящий
/// (`ComponentId == tap0901`) с извлечёнными `NetCfgInstanceId` (GUID) и `Name`.
#[must_use]
#[cfg(windows)]
pub fn find_tap_adapter() -> Option<TapAdapterInfo> {
    find_tap_adapter_inner().ok().flatten()
}

#[cfg(not(windows))]
pub fn find_tap_adapter() -> Option<TapAdapterInfo> {
    None
}

#[cfg(windows)]
fn find_tap_adapter_inner() -> Result<Option<TapAdapterInfo>, TapError> {
    let class_path = encode_wide(NET_CLASS_KEY);
    let mut class_key: HKEY = std::ptr::null_mut();
    // SAFETY: RegOpenKeyExW открывает ключ HKLM\...\Class; валиден до RegCloseKey.
    let status = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, class_path.as_ptr(), 0, KEY_READ, &mut class_key)
    };
    if status != 0 {
        return Err(TapError::Registry(format!(
            "RegOpenKeyExW Class key failed (err={status})"
        )));
    }

    let mut found: Option<TapAdapterInfo> = None;
    let mut idx: u32 = 0;
    loop {
        let mut sub_name = [0u16; 64];
        // SAFETY: RegEnumKeyW перечисляет сабкеи; буфер 64 wchars достаточен
        // для имён вида "0007".
        // cast: sub_name.len() = 64, безопасно в u32.
        #[allow(clippy::cast_possible_truncation)]
        let cap_u32: u32 = sub_name.len() as u32;
        let status =
            unsafe { RegEnumKeyW(class_key, idx, sub_name.as_mut_ptr(), cap_u32) };
        if status == 259 {
            break; // ERROR_NO_MORE_ITEMS
        }
        if status == 0 {
            let name_len = sub_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(sub_name.len());
            let sub_name_str = String::from_utf16_lossy(&sub_name[..name_len]);
            if let Some(info) = probe_subkey(class_key, &sub_name_str) {
                found = Some(info);
                break;
            }
        }
        idx += 1;
    }

    // SAFETY: закрываем дескриптор ключа, открытый выше.
    unsafe { RegCloseKey(class_key) };
    Ok(found)
}

#[cfg(windows)]
fn probe_subkey(parent: HKEY, sub_name: &str) -> Option<TapAdapterInfo> {
    let sub_w = encode_wide(sub_name);
    let mut sub_key: HKEY = std::ptr::null_mut();
    // SAFETY: RegOpenKeyExW на сабкей; валиден до RegCloseKey ниже.
    let status = unsafe { RegOpenKeyExW(parent, sub_w.as_ptr(), 0, KEY_READ, &mut sub_key) };
    if status != 0 {
        return None;
    }

    // Нет ComponentId → это не сетевой адаптер в нашем смысле; пропускаем.
    let component_id = query_reg_string(sub_key, "ComponentId");
    let result = (|| {
        let component_id = component_id.as_ref()?;
        if !component_id.eq_ignore_ascii_case(TAP_COMPONENT_ID) {
            return None;
        }
        let guid = query_reg_string(sub_key, "NetCfgInstanceId").unwrap_or_default();
        let name = query_reg_string(sub_key, "Name")
            .unwrap_or_else(|| format!("Lattice-{guid}"));
        Some(TapAdapterInfo { guid, name })
    })();

    // SAFETY: закрываем дескриптор сабкея.
    unsafe { RegCloseKey(sub_key) };
    result
}

/// Прочитать REG_SZ-значение `name` из открытого ключа `key`. `None` если
/// значения нет, тип не `REG_SZ`, или буфер 1024 байта недостаточен (`PoC` не
/// интересуют values длиннее). Чистая функция — никогда не падает, поэтому
/// `Option`, не `Result`.
#[cfg(windows)]
fn query_reg_string(key: HKEY, name: &str) -> Option<String> {
    let name_w = encode_wide(name);
    let mut buf = [0u16; 512];
    // cast: buf.len()*2 = 1024, безопасно в u32.
    #[allow(clippy::cast_possible_truncation)]
    let buf_bytes_u32: u32 = (buf.len() * 2) as u32;
    let mut buf_len: u32 = buf_bytes_u32;
    let mut reg_type: u32 = 0;
    // SAFETY: RegQueryValueExW читает REG_SZ. Буфер 512 wchars = 1024 байта —
    // достаточно для GUID/Name; если значение длиннее, вернём None.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut reg_type,
            buf.as_mut_ptr().cast::<u8>(),
            &mut buf_len,
        )
    };
    if status != 0 || reg_type != REG_SZ {
        return None;
    }
    // cast: buf_len ≤ 1024, безопасно в usize (u32 → usize — extensional cast).
    let words = (buf_len as usize) / 2;
    let trimmed = buf[..words]
        .iter()
        .position(|&c| c == 0)
        .map_or(&buf[..words], |n| &buf[..n]);
    Some(
        String::from_utf16_lossy(trimmed)
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// Закодировать &str в UTF-16 с NUL-терминатором (для W-вариантов Win32 API).
/// Используется и из `mod.rs` (путь к устройству), и здесь (реестр).
#[cfg(windows)]
pub(crate) fn encode_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsString::from(s).encode_wide().collect();
    v.push(0);
    v
}
