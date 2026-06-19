//! Маскирующие меры Фазы 4: padding длин + timing jitter. За тем же
//! `trait Transport`, включаются независимо от выбора UDP/QUIC.
//!
//! ## Что это даёт (честная планка)
//!
//! Padding ломает характерное распределение ДЛИН (особенно мелкие keepalive/
//! punch-пакеты фиксированного размера — яркая сигнатура). Jitter ломает
//! машинно-регулярный РИТМ служебных пакетов. Вместе они убирают два дешёвых
//! пассивных признака. Это НЕ делает поток неотличимым от HTTP/3 под активным
//! пробингом — только убирает грубую эвристику (см. AGENTS.md «Честная планка»).
//!
//! ## Цена (в коде помечено)
//!
//! - Padding раздувает трафик: cap на оверхед (`MaxPadding`), не паддим то, что
//!   и так крупное (≥ `PadTarget`). Дефолт паддит только мелочь.
//! - Jitter добавляет задержку: для датаплейна осторожнее (cap мал), для
//!   signaling/keepalive можно щедрее — там latency некритична.
//!
//! ## Формат обёртки и согласование
//!
//! Padding меняет wire-формат: `[u16 BE real_len][payload][random padding]`.
//! Значит ОБА пира должны включить obfs — иначе приёмник не снимет обёртку.
//! Согласование — через signaling (один флаг на сеть/пару), не молчаливый
//! рассинхрон. Внутренний слой (`[nonce||AEAD(frame)]`) padding НЕ трогает —
//! это внешняя обёртка поверх уже зашифрованного; E2E не затрагивается, relay
//! по-прежнему видит только ciphertext внутреннего слоя.

use std::net::SocketAddr;
use std::time::Duration;

use rand::Rng;

use crate::transport::{Transport, TransportError};

/// До какого размера добивать мелкие датаграммы (байт, включая 2-байтный
/// префикс длины). Datagram ≥ этого не паддится. `0` — padding выключен.
#[derive(Debug, Clone, Copy)]
pub struct PadTarget(pub usize);

/// Потолок добавляемых байт на одну датаграмму — чтобы padding не раздул трафик
/// безгранично (напр. большой фрейм не добивается до следующего «типичного»
/// размера ценой ×2 оверхеда).
#[derive(Debug, Clone, Copy)]
pub struct MaxPadding(pub usize);

/// Политика padding. Дефолт: добивать до 1200 байт (типичный размер QUIC-
/// Initial / медиапакета), но не добавлять больше 512 байт за раз.
#[derive(Debug, Clone, Copy)]
pub struct PaddingPolicy {
    pub target: PadTarget,
    pub max_extra: MaxPadding,
}

impl Default for PaddingPolicy {
    fn default() -> Self {
        Self {
            target: PadTarget(1200),
            max_extra: MaxPadding(512),
        }
    }
}

impl PaddingPolicy {
    /// Выключенный padding (passthrough) — wire-формат всё равно с префиксом,
    /// так что обёртка применяется, но лишних байт не добавляется.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            target: PadTarget(0),
            max_extra: MaxPadding(0),
        }
    }
}

/// 2-байтный BE-префикс длины полезной нагрузки.
const LEN_PREFIX: usize = 2;

/// Обернуть payload: `[u16 real_len][payload][padding]`. Возвращает `Err`, если
/// payload не влезает в `u16` (наш датаплейн ≤ MTU, так не бывает, но не паникуем).
///
/// # Errors
///
/// `TransportError::Io` с `InvalidInput`, если `payload.len() > u16::MAX`.
pub fn wrap(payload: &[u8], policy: PaddingPolicy) -> Result<Vec<u8>, TransportError> {
    let real_len = u16::try_from(payload.len()).map_err(|_| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "obfs payload exceeds u16 length",
        ))
    })?;
    let framed = LEN_PREFIX + payload.len();
    // Паддим только то, что мельче target; cap на добавку. Крупное не трогаем.
    let pad = policy
        .target
        .0
        .saturating_sub(framed)
        .min(policy.max_extra.0);
    let mut out = Vec::with_capacity(framed + pad);
    out.extend_from_slice(&real_len.to_be_bytes());
    out.extend_from_slice(payload);
    if pad > 0 {
        // Случайные байты, не нули: нулевой хвост сам по себе сигнатура.
        let start = out.len();
        out.resize(start + pad, 0);
        rand::thread_rng().fill(&mut out[start..]);
    }
    Ok(out)
}

/// Снять обёртку in-place: читает префикс длины, сдвигает payload в начало
/// `buf`, отбрасывает padding. Возвращает длину payload или `None` (битый кадр).
#[must_use]
pub fn unwrap_in_place(buf: &mut [u8], n: usize) -> Option<usize> {
    if n < LEN_PREFIX {
        return None;
    }
    let real_len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if LEN_PREFIX + real_len > n {
        return None; // объявленная длина больше принятого — мусор/обрезка.
    }
    buf.copy_within(LEN_PREFIX..LEN_PREFIX + real_len, 0);
    Some(real_len)
}

/// Транспорт-обёртка: паддит исходящее, снимает обёртку входящего. Generic по
/// нижележащему транспорту (UDP или QUIC) — маскировка не зависит от того, что
/// под ней. `inner` — owned, чтобы обёртка владела сокетом/соединением.
pub struct ObfsTransport<T: Transport> {
    inner: T,
    padding: PaddingPolicy,
}

impl<T: Transport> ObfsTransport<T> {
    #[must_use]
    pub fn new(inner: T, padding: PaddingPolicy) -> Self {
        Self { inner, padding }
    }
}

impl<T: Transport> Transport for ObfsTransport<T> {
    fn send(&self, addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        let framed = wrap(data, self.padding)?;
        self.inner.send(addr, &framed)
    }

    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        let (n, from) = self.inner.recv(buf)?;
        match unwrap_in_place(buf, n) {
            Some(len) => Ok((len, from)),
            // Битая обёртка (не от obfs-пира / порча) — как таймаут, не падаем.
            None => Err(TransportError::WouldBlock),
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Политика timing jitter для служебных пакетов (keepalive/relay-hello).
/// Рандомизирует интервал в `[base - spread, base + spread]`, но не ниже
/// `floor` (слишком частый keepalive сам по себе сигнатура и грузит сеть).
#[derive(Debug, Clone, Copy)]
pub struct JitterPolicy {
    pub base: Duration,
    pub spread: Duration,
    pub floor: Duration,
}

impl JitterPolicy {
    /// Без jitter — всегда `base` (детерминированный ритм).
    #[must_use]
    pub fn fixed(base: Duration) -> Self {
        Self {
            base,
            spread: Duration::ZERO,
            floor: base,
        }
    }

    /// Следующий интервал с jitter. Cap снизу — `floor` (см. выше), сверху —
    /// `base + spread` (чтобы не уехать за NAT-таймаут для keepalive).
    #[must_use]
    pub fn next_interval(&self) -> Duration {
        if self.spread.is_zero() {
            return self.base;
        }
        let spread_ms = u64::try_from(self.spread.as_millis()).unwrap_or(u64::MAX);
        let base_ms = u64::try_from(self.base.as_millis()).unwrap_or(u64::MAX);
        // Равномерно в [base-spread, base+spread]; saturating, чтобы не уйти в 0.
        let lo = base_ms.saturating_sub(spread_ms);
        let hi = base_ms.saturating_add(spread_ms);
        let picked = if lo >= hi {
            base_ms
        } else {
            rand::thread_rng().gen_range(lo..=hi)
        };
        Duration::from_millis(picked).max(self.floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip_small() {
        let payload = b"keepalive";
        let mut framed = wrap(payload, PaddingPolicy::default()).expect("wrap");
        // Мелкий пакет добит до target.
        assert!(framed.len() >= 100, "small packet should be padded, got {}", framed.len());
        let n = framed.len();
        let len = unwrap_in_place(&mut framed, n).expect("unwrap");
        assert_eq!(&framed[..len], payload);
    }

    #[test]
    fn wrap_does_not_pad_large() {
        let payload = vec![0xAB_u8; 1400];
        let framed = wrap(&payload, PaddingPolicy::default()).expect("wrap");
        // 1400 + 2 ≥ target(1200) → без добавки.
        assert_eq!(framed.len(), payload.len() + LEN_PREFIX);
    }

    #[test]
    fn wrap_caps_overhead() {
        let policy = PaddingPolicy {
            target: PadTarget(10_000),
            max_extra: MaxPadding(100),
        };
        let framed = wrap(b"x", policy).expect("wrap");
        // target огромный, но добавка ограничена max_extra.
        assert_eq!(framed.len(), LEN_PREFIX + 1 + 100);
    }

    #[test]
    fn disabled_padding_only_adds_prefix() {
        let framed = wrap(b"hello", PaddingPolicy::disabled()).expect("wrap");
        assert_eq!(framed.len(), LEN_PREFIX + 5);
    }

    #[test]
    fn unwrap_rejects_truncated() {
        let mut buf = [0u8; 8];
        buf[0] = 0xFF; // объявленная длина 0xFF00, а принято 8 байт → мусор.
        buf[1] = 0x00;
        assert_eq!(unwrap_in_place(&mut buf, 8), None);
        assert_eq!(unwrap_in_place(&mut buf, 1), None); // короче префикса.
    }

    #[test]
    fn jitter_within_bounds_and_floor() {
        let j = JitterPolicy {
            base: Duration::from_secs(1),
            spread: Duration::from_millis(300),
            floor: Duration::from_millis(800),
        };
        for _ in 0..200 {
            let d = j.next_interval();
            assert!(d >= Duration::from_millis(800), "below floor: {d:?}");
            assert!(d <= Duration::from_millis(1300), "above base+spread: {d:?}");
        }
    }

    #[test]
    fn fixed_jitter_is_deterministic() {
        let j = JitterPolicy::fixed(Duration::from_secs(20));
        assert_eq!(j.next_interval(), Duration::from_secs(20));
    }
}
