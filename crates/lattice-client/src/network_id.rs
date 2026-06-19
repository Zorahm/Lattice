//! `network-id = BLAKE3(shared-key)` (Фаза 3).
//!
//! **Крипто-контракт:** ключ сети живёт ТОЛЬКО на клиенте. Здесь он хэшируется
//! в `NetworkId` (32 байта → 64 hex), который предъявляется coordination-серверу.
//! Сервер сводит пиров с одинаковым `network-id`, но самого ключа не видит и не
//! раздаёт — E2E из Фаз 1-2 не ослабляется. В коде сервера ключа физически нет.
//!
//! BLAKE3 выбран как современный быстрый хэш; `blake3` crate не тянет
//! `windows-sys` (SIMD через `std::arch`, не платформенные крейты) — контракт
//! `cargo tree` для сервера соблюдён (хэш-библиотека — в клиенте, не в сервере).

use lattice_proto::{NetworkId, NETWORK_ID_HEX_LEN};

use crate::crypto::Key;

/// Вычислить `network-id` из shared-ключа: `BLAKE3(key)` → 32 байта → hex.
///
/// # Errors
///
/// `String` — если hex-кодирование провалилось (на корректной системе не
/// случается; `hex::encode` всегда работает для `[u8; 32]`).
pub fn from_key(key: &Key) -> Result<NetworkId, String> {
    let hash = blake3::hash(key.as_bytes());
    // `blake3::hash` возвращает 32-байтный `Hash`; `.as_bytes()` — `&[u8; 32]`.
    let hex = hex::encode(hash.as_bytes());
    debug_assert_eq!(
        hex.len(),
        NETWORK_ID_HEX_LEN,
        "BLAKE3-256 hex must be {NETWORK_ID_HEX_LEN} chars"
    );
    NetworkId::from_hex(&hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::KEY_LEN;

    #[test]
    fn produces_valid_network_id() {
        let key = Key::new([0x42; KEY_LEN]);
        let nid = from_key(&key).expect("valid network-id");
        assert_eq!(nid.as_str().len(), NETWORK_ID_HEX_LEN);
        assert!(nid.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn same_key_same_id() {
        let key = Key::new([0xAB; KEY_LEN]);
        assert_eq!(from_key(&key).unwrap(), from_key(&key).unwrap());
    }

    #[test]
    fn different_keys_different_ids() {
        let a = from_key(&Key::new([0x01; KEY_LEN])).unwrap();
        let b = from_key(&Key::new([0x02; KEY_LEN])).unwrap();
        assert_ne!(a, b);
    }
}
