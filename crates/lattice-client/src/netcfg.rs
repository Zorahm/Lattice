//! Назначение IP-адреса и MTU на TAP-адаптере. PoC-путь — `netsh` (доступен
//! из коробки на Windows, не требует доп. зависимостей). SPEC допускает это;
//! MVP при желании перейдёт на IP Helper API (`CreateUnicastIpAddressEntry`),
//! но это не меняет контракт модуля — только реализацию внутри.
//!
//! Здесь же — проверка прав администратора: поднятие линка / netsh требуют
//! elevation, и пользователь должен получить внятное сообщение, а не
//! непонятный отказ драйвера.

use std::net::Ipv4Addr;
use std::process::Command;

use thiserror::Error;

/// MTU виртуального адаптера. 1380, не 1500: после инкапсуляции
/// (nonce 12 + AEAD-тег 16 + UDP 8 + IP 20 ≈ 56 байт оверхеда) датаграмма
/// не должна превышать 1500 на физическом линке — иначе фрагментация, которую
/// многие NAT/DPI режут. См. AGENTS.md «MTU ~1380».
pub const TAP_MTU: u32 = 1380;

/// Длина префикса подсети в битах для IPv4. PoC-сеть — /24 (10.66.0.0/24).
pub const DEFAULT_PREFIX_LEN: u8 = 24;

/// Ошибка конфигурации сети. Все пути, которые могут провалиться из-за
/// окружения (нет прав, нет адаптера, netsh вернул ошибку), дают осмысленный
/// контекст — не голый код выхода.
#[derive(Debug, Error)]
pub enum NetcfgError {
    #[error("not running as administrator; relaunch elevated (Run as Administrator)")]
    NotElevated,
    #[error("netsh invocation failed: {0}")]
    Spawn(String),
    #[error("netsh exited with code {0}: {1}")]
    NetshExit(i32, String),
    #[error("invalid CIDR: {0}/{1}")]
    InvalidCidr(Ipv4Addr, u8),
}

/// Проверить, что процесс запущен с правами администратора. Делегирует в
/// `tap::check_admin` — это Win32-вызов (`CheckTokenMembership`), а весь
/// `unsafe`/Win32 по контракту AGENTS.md живёт только в модуле `tap`.
///
/// # Errors
///
/// `NotElevated` — запуск без elevation; `Spawn` — Win32-ошибка при работе с
/// токеном (нет смысла продолжать, но это не паника — диагностируем сообщение).
pub fn check_admin() -> Result<(), NetcfgError> {
    crate::tap::check_admin().map_err(|e| match e {
        crate::tap::TapError::NotElevated => NetcfgError::NotElevated,
        other => NetcfgError::Spawn(other.to_string()),
    })
}

/// Назначить статический IPv4-адрес и MTU на адаптере с именем `iface`.
/// Делается через `netsh interface ip` / `netsh interface ipv4 set subinterface`.
///
/// # Errors
///
/// `InvalidCidr` — префикс вне диапазона 1..=32. `Spawn` — не удалось запустить
/// `netsh`. `NetshExit` — `netsh` завершился ненулём (контекст в сообщении).
pub fn configure_interface(
    iface: &str,
    ip: Ipv4Addr,
    prefix_len: u8,
    mtu: u32,
) -> Result<(), NetcfgError> {
    if prefix_len == 0 || prefix_len > 32 {
        return Err(NetcfgError::InvalidCidr(ip, prefix_len));
    }
    let mask = cidr_to_netmask(prefix_len)?;
    let ip_str = ip.to_string();

    // netsh interface ip set address name="<iface>" source=static
    //   addr=<ip> mask=<mask>. Без gateway — у PoC-сети нет шлюза, пирская mesh.
    run_netsh(&[
        "interface", "ip", "set", "address",
        &format!("name={iface}"),
        "source=static",
        &format!("addr={ip_str}"),
        &format!("mask={mask}"),
    ])?;

    // MTU на subinterface. store=persistent — переживает перезапуск адаптера.
    run_netsh(&[
        "interface", "ipv4", "set", "subinterface",
        iface,
        &format!("mtu={mtu}"),
        "store=persistent",
    ])?;

    Ok(())
}

fn cidr_to_netmask(prefix: u8) -> Result<String, NetcfgError> {
    if prefix == 0 {
        return Ok("0.0.0.0".to_string());
    }
    if prefix > 32 {
        return Err(NetcfgError::InvalidCidr(Ipv4Addr::UNSPECIFIED, prefix));
    }
    let bits: u32 = (!0u32) << (32 - prefix);
    let mask = bits.to_be_bytes();
    Ok(format!("{}.{}.{}.{}", mask[0], mask[1], mask[2], mask[3]))
}

/// Запустить `netsh` с аргументами, каждое значение — отдельный токен. При
/// ненулевом выходе возвращает stderr+stdout, чтобы ошибка имела контекст.
fn run_netsh(args: &[&str]) -> Result<(), NetcfgError> {
    let output = Command::new("netsh")
        .args(args)
        .output()
        .map_err(|e| NetcfgError::Spawn(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
            combined.push('\n');
        }
        combined.push_str(&stderr);
        let code = output.status.code().unwrap_or(-1);
        return Err(NetcfgError::NetshExit(code, combined.trim().to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netmask_24() {
        assert_eq!(cidr_to_netmask(24).unwrap(), "255.255.255.0");
    }

    #[test]
    fn netmask_16() {
        assert_eq!(cidr_to_netmask(16).unwrap(), "255.255.0.0");
    }

    #[test]
    fn netmask_32() {
        assert_eq!(cidr_to_netmask(32).unwrap(), "255.255.255.255");
    }

    #[test]
    fn netmask_0() {
        assert_eq!(cidr_to_netmask(0).unwrap(), "0.0.0.0");
    }

    #[test]
    fn netmask_invalid() {
        assert!(cidr_to_netmask(33).is_err());
    }
}
