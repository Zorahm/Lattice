//! AEAD-обёртка над ChaCha20-Poly1305 для UDP-датаплейна.
//!
//! Wire-формат датаграммы (см. SPEC.md «Формат UDP-датаграммы»):
//! `[ nonce: 12 байт ][ ChaCha20-Poly1305(ethernet_frame): N+16 байт ]`.
//!
//! Nonce генерируется случайно на каждый фрейм через `OsRng` — повтор nonce
//! под тем же ключом даёт катастрофическую потерю конфиденциальности, поэтому
//! берём криптографический источник, а не счётчик (счётчик требовал бы
//! синхронизации между воркерами и персистентности при рестарте).

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key as CipherKey, Nonce as CipherNonce,
};
use rand::rngs::OsRng;
use rand::RngCore;
use thiserror::Error;

/// Длина pre-shared ключа ChaCha20-Poly1305 в байтах.
pub const KEY_LEN: usize = 32;
/// Длина nonce AEAD в байтах.
pub const NONCE_LEN: usize = 12;
/// Длина AEAD-тега Poly1305 в байтах.
pub const TAG_LEN: usize = 16;
/// Минимальная длина валидной датаграммы: nonce + пустой plaintext + тег.
pub const MIN_DATAGRAM_LEN: usize = NONCE_LEN + TAG_LEN;

/// Newtype ключа: 32 байта. Не даёт перепутать ключ с любым другим `[u8; 32]`
/// (например, с nonce-буфером или хэшем) на уровне типа.
#[derive(Clone)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    #[must_use]
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// Newtype nonce: 12 байт. Нужен чтобы не передать в `open` случайно кусок
/// ciphertext'а как nonce — типы разные.
#[derive(Clone, Copy)]
pub struct Nonce([u8; NONCE_LEN]);

impl Nonce {
    fn random() -> Self {
        let mut buf = [0u8; NONCE_LEN];
        // OsRng — системный CSPRNG; для nonce это единственный допустимый
        // источник при отсутствии синхронизированного счётчика.
        OsRng.fill_bytes(&mut buf);
        Self(buf)
    }

    fn from_slice(src: &[u8]) -> Option<Self> {
        if src.len() != NONCE_LEN {
            return None;
        }
        let mut buf = [0u8; NONCE_LEN];
        buf.copy_from_slice(src);
        Some(Self(buf))
    }
}

/// Ошибки криптографического слоя. В горячем пути `open` возвращает `None`
/// (чтобы цикл приёма просто дропал чужие/битые пакеты), а `seal` — `Result`,
/// потому что неудача seal это инвариантное нарушение (отказ RNG), не чужой
/// пакет, и должно громко диагностироваться.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// Датаграмма короче nonce+тега — в ней гарантированно нет ни nonce, ни
    /// целого тега; расшифровка бессмысленна.
    #[error("datagram too short: {0} bytes, need at least {1}")]
    DatagramTooShort(usize, usize),
    /// AEAD-тег не сошёлся — чужой пакет, другой ключ, битые данные. В горячем
    /// пути это норма (сканирование порта / мусор), логировать на trace, не падать.
    #[error("AEAD authentication failed")]
    AuthFailed,
    /// Внутренняя ошибка AEAD при seal — означает отказ RNG или невозможность
    /// выделить буфер. На корректной системе не должна случаться.
    #[error("encrypt failure: {0}")]
    Encrypt(String),
}

/// Симметричный шифратор датаплейна. Потокобезопасен через `&self` (cipher
/// внутри `ChaCha20Poly1305` не имеет мутабельного состояния при encrypt/decrypt).
pub struct Crypto {
    cipher: ChaCha20Poly1305,
}

impl Crypto {
    #[must_use]
    pub fn new(key: &Key) -> Self {
        // KeyInit::new берёт &[u8; 32]; клонируем во внутренний cipher.
        Self {
            cipher: ChaCha20Poly1305::new(CipherKey::from_slice(key.as_bytes())),
        }
    }

    /// Зашифровать Ethernet-фрейм → UDP-датаграмма `[nonce || ciphertext+tag]`.
    /// Не падает на больших фреймах: capacity закладываем сразу.
    ///
    /// # Errors
    ///
    /// Возвращает `CryptoError::Encrypt` только при внутреннем отказе AEAD
    /// (RNG / аллокация), на корректной системе не должно случаться.
    pub fn seal(&self, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = Nonce::random();
        let ct = self
            .cipher
            .encrypt(CipherNonce::from_slice(&nonce.0), frame)
            .map_err(|e| CryptoError::Encrypt(e.to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce.0);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Расшифровать UDP-датаграмму → Ethernet-фрейм.
    ///
    /// Возвращает `None` на любых отброшенных пакетах (короткие / неверный
    /// AEAD-тег) — это сигнал вызывающему циклу «дроп молча, без паники»,
    /// см. AGENTS.md edge cases. Подробности доступны через `open_detailed`,
    /// если когда-то понадобится различать причины для метрик.
    #[must_use]
    pub fn open(&self, datagram: &[u8]) -> Option<Vec<u8>> {
        self.open_detailed(datagram).ok()
    }

    /// То же, что `open`, но с типизированной причиной отбраковки — для
    /// неторопливого пути (логирование / счётчики).
    ///
    /// # Errors
    ///
    /// `DatagramTooShort` — короче `MIN_DATAGRAM_LEN` (нет nonce или тега).
    /// `AuthFailed` — AEAD-тег не сошёлся (чужой/битый пакет).
    pub fn open_detailed(&self, datagram: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if datagram.len() < MIN_DATAGRAM_LEN {
            return Err(CryptoError::DatagramTooShort(
                datagram.len(),
                MIN_DATAGRAM_LEN,
            ));
        }
        let (nonce_bytes, ct) = datagram.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes)
            .ok_or(CryptoError::DatagramTooShort(nonce_bytes.len(), NONCE_LEN))?;
        // AEAD-тег проверяется внутри decrypt: чужие/битые пакеты возвращают
        // `Err`. Ручная валидация nonce не нужна и была бы вредна — любой
        // nonce под корректным тегом легитимен.
        self.cipher
            .decrypt(CipherNonce::from_slice(&nonce.0), ct)
            .map_err(|_| CryptoError::AuthFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_from(seed: u8) -> Key {
        Key::new([seed; KEY_LEN])
    }

    #[test]
    fn roundtrip_small_frame() {
        let c = Crypto::new(&key_from(0xA5));
        let frame = b"hello lattice";
        let dg = c.seal(frame).expect("seal");
        let back = c.open(&dg).expect("open");
        assert_eq!(back, frame);
    }

    #[test]
    fn roundtrip_empty_frame() {
        let c = Crypto::new(&key_from(0x00));
        let dg = c.seal(&[]).expect("seal");
        // nonce(12) + tag(16) = 28 минимально.
        assert_eq!(dg.len(), MIN_DATAGRAM_LEN);
        assert!(c.open(&dg).is_some());
    }

    #[test]
    fn datagram_too_short_dropped() {
        let c = Crypto::new(&key_from(0x01));
        // 11 байт < nonce, но даже < MIN_DATAGRAM_LEN.
        assert!(c.open(&[0u8; 11]).is_none());
        assert!(c.open(&[0u8; NONCE_LEN]).is_none()); // nonce есть, тега нет.
    }

    #[test]
    fn tampered_tag_dropped_silently() {
        let c = Crypto::new(&key_from(0x42));
        let mut dg = c.seal(b"payload").expect("seal");
        // Флип последнего байта тега.
        let last = dg.len() - 1;
        dg[last] ^= 0xFF;
        assert!(c.open(&dg).is_none());
    }

    #[test]
    fn wrong_key_rejects() {
        let a = Crypto::new(&key_from(0x10));
        let b = Crypto::new(&key_from(0x11));
        let dg = a.seal(b"secret").expect("seal");
        assert!(b.open(&dg).is_none());
    }

    #[test]
    fn nonces_differ_across_seals() {
        let c = Crypto::new(&key_from(0x77));
        let a = c.seal(b"x").expect("seal");
        let b = c.seal(b"x").expect("seal");
        // Два nonce подряд не должны совпадать — иначе повтор nonce под тем
        // же ключом ломает конфиденциальность.
        assert_ne!(&a[..NONCE_LEN], &b[..NONCE_LEN]);
    }
}
