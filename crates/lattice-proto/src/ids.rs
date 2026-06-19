//! Newtype-идентификаторы Фазы 3: `NetworkId`, `PeerId`, `OverlayIp`.
//!
//! Зачем newtype: семантически разные значения (хэш ключа, id пира, overlay-IP)
//! не должны смешиваться на уровне типа — голые `String` давали бы спутать
//! `network_id` с `peer_id` в сигнатуре. Каждый newtype валидирует своё
//! содержимое при конструировании, чтобы инвариант держался везде, где значение
//! проходит через wire.
//!
//! Все три хранятся строками, чтобы крейт остался `no_std + alloc` (без
//! `std::net::Ipv4Addr`/`uuid`). Парсинг в нативные типы — на стороне
//! client/server, где `std` есть.

use alloc::format;
use alloc::string::{String, ToString};
use serde::{Deserialize, Serialize};

/// Длина `NetworkId` в байтах: BLAKE3 от 32-байтного shared-ключа даёт 32 байта.
/// На wire едет hex-кодированной строкой (64 символа) — так proto не тянет
/// массивы и остаётся `serde`-only.
pub const NETWORK_ID_HEX_LEN: usize = 64;

/// Идентификатор сети = `BLAKE3(shared-key)` (32 байта → 64 hex-символа).
///
/// **Крипто-контракт Фазы 3:** клиент вычисляет его ЛОКАЛЬНО (где живёт ключ) и
/// предъявляет серверу ТОЛЬКО хэш. Сервер сводит пиров с одинаковым `network-id`,
/// но самого ключа не видит и не раздаёт — E2E из Фаз 1-2 не ослабляется. В коде
/// сервера ключ сети физически отсутствует.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NetworkId(String);

impl NetworkId {
    /// Построить из hex-строки. `Err`, если длина ≠ 64 или не hex — вызывающий
    /// (клиент) должен дать корректный id, сервер при приёме тоже валидирует.
    ///
    /// # Errors
    ///
    /// Текстовая ошибка при неверной длине/символах.
    pub fn from_hex(hex: &str) -> Result<Self, String> {
        if hex.len() != NETWORK_ID_HEX_LEN {
            return Err(format!(
                "network-id must be {NETWORK_ID_HEX_LEN} hex chars, got {}",
                hex.len()
            ));
        }
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("network-id contains non-hex characters".to_string());
        }
        // Нормализуем к lowercase, чтобы сравнение строк на сервере было
        // регистронезависимым без отдельной логики.
        Ok(Self(hex.to_ascii_lowercase()))
    }

    /// Без проверки — только из заведомо корректного источника (тесты/внутреннее
    /// построение). В production-коде предпочитайте `from_hex`.
    #[must_use]
    pub fn new_unchecked(hex: &str) -> Self {
        Self(hex.to_ascii_lowercase())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NetworkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Идентификатор пира в сети. Клиент генерирует его локально (произвольная
/// строка, напр. UUID/hostname+pid), сервер использует как ключ в реестре и
/// адресует апдейты (`PeerJoined`/`PeerLeft`/`PeerUpdated`). Newtype, чтобы не
/// путать со случайной строкой (endpoint, network-id и т.п.).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerId(String);

impl PeerId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Overlay-IP пира в виртуальной сети (self-assigned клиентом, напр.
/// `10.66.0.5`). Хранится строкой, чтобы proto остался `no_std`. Сервер хранит
/// его для отображения в `WebUI` и детекта коллизий (два пира в одной сети с
/// одинаковым overlay-IP → `Error` второму).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OverlayIp(String);

impl OverlayIp {
    #[must_use]
    pub fn new(ip: String) -> Self {
        Self(ip)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OverlayIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_id_accepts_valid_hex() {
        let s = "ab".repeat(NETWORK_ID_HEX_LEN / 2);
        assert!(NetworkId::from_hex(&s).is_ok());
    }

    #[test]
    fn network_id_rejects_wrong_length() {
        assert!(NetworkId::from_hex("ab").is_err());
    }

    #[test]
    fn network_id_rejects_non_hex() {
        let mut bad = "ab".repeat(NETWORK_ID_HEX_LEN / 2);
        bad.insert(0, 'z');
        bad.truncate(NETWORK_ID_HEX_LEN);
        assert!(NetworkId::from_hex(&bad).is_err());
    }

    #[test]
    fn network_id_normalizes_case() {
        let a = NetworkId::from_hex(&"AB".repeat(NETWORK_ID_HEX_LEN / 2)).expect("upper");
        let b = NetworkId::from_hex(&"ab".repeat(NETWORK_ID_HEX_LEN / 2)).expect("lower");
        assert_eq!(a, b);
    }
}
