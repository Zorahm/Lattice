//! Формат relay-обёртки датаплейна (UDP клиент↔сервер↔клиент).
//!
//! Relay включается, когда punch не сошёлся. Сервер — тупой UDP-ретранслятор:
//! получив пакет на relay-сокете, по `session` находит комнату и пересылает
//! `payload` другому её участнику. **Ключа сервер не имеет — `payload` это
//! ровно датаплейн-датаграмма `[nonce || ChaCha20-Poly1305(frame)]`, сервер
//! видит только ciphertext** (контракт E2E из SPEC/AGENTS не ослабляется).
//!
//! Обёртка — голые байты, не serde: датаплейн горячий, JSON здесь был бы
//! расточительством, а формат тривиален и фиксирован.
//!
//! ```text
//! [ magic: 4 ][ session: 8 (big-endian u64) ][ payload: N (ciphertext) ]
//! ```
//!
//! `payload` длины 0 — «hello/keepalive»: клиент шлёт его сразу при переходе в
//! relay, чтобы сервер узнал его внешний адрес (source UDP-пакета) ДО того, как
//! пойдут данные — иначе серверу некуда пересылать пакеты второго пира.

use alloc::vec::Vec;

/// Магическая сигнатура relay-пакета. Отсекает случайный мусор / сканеры,
/// прилетевший на relay-порт (не security-граница — ей служит AEAD-тег внутри
/// payload, который сервер не проверяет и проверить не может).
pub const RELAY_MAGIC: [u8; 4] = *b"LRLY";

/// Размер фиксированного заголовка: magic(4) + session(8).
pub const RELAY_HEADER_LEN: usize = 4 + 8;

/// Завернуть `payload` в relay-обёртку для отправки на relay-сокет сервера.
#[must_use]
pub fn encode(session: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RELAY_HEADER_LEN + payload.len());
    out.extend_from_slice(&RELAY_MAGIC);
    out.extend_from_slice(&session.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Разобрать relay-пакет → `(session, payload)`. `None`, если короче заголовка
/// или сигнатура не совпала — вызывающий молча дропает (чужой/битый пакет).
#[must_use]
pub fn decode(buf: &[u8]) -> Option<(u64, &[u8])> {
    if buf.len() < RELAY_HEADER_LEN || buf[..4] != RELAY_MAGIC {
        return None;
    }
    // 8 байт session big-endian сразу после magic; срез фиксированной длины →
    // try_into не может провалиться, но обрабатываем Result без unwrap.
    let session_bytes: [u8; 8] = buf[4..RELAY_HEADER_LEN].try_into().ok()?;
    let session = u64::from_be_bytes(session_bytes);
    Some((session, &buf[RELAY_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dg = encode(0xDEAD_BEEF_0000_0001, b"ciphertext");
        let (session, payload) = decode(&dg).expect("decode");
        assert_eq!(session, 0xDEAD_BEEF_0000_0001);
        assert_eq!(payload, b"ciphertext");
    }

    #[test]
    fn empty_payload_is_hello() {
        let dg = encode(7, &[]);
        let (session, payload) = decode(&dg).expect("decode");
        assert_eq!(session, 7);
        assert!(payload.is_empty());
    }

    #[test]
    fn rejects_short() {
        assert!(decode(&[0u8; RELAY_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut dg = encode(1, b"x");
        dg[0] ^= 0xFF;
        assert!(decode(&dg).is_none());
    }
}
