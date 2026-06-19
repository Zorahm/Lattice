//! UDP hole punching между двумя пирами + keepalive NAT-биндинга.
//!
//! ## Почему синхронный старт
//!
//! NAT пропускает входящий UDP только после исходящего на тот же внешний адрес
//! (он создаёт маппинг/разрешение). Если шлёт только один пир, его первые
//! пакеты упрутся в закрытый NAT второго. Поэтому ОБА начинают слать примерно
//! одновременно — по go-сигналу сервера (`ServerMessage::Start`): каждый своим
//! исходящим пробивает разрешение, и встречные пакеты проходят. Точная
//! синхронизация часов не нужна — bursts длятся секунды, перекрытие гарантировано.
//!
//! ## Почему punch-пакеты шифруются
//!
//! Контрольные пакеты идут через тот же AEAD (`crypto.seal`), что и данные:
//! - только владелец ключа может их отправить → off-path атакующий не угонит
//!   и не подделает punch (нельзя «увести» соединение чужим pong'ом);
//! - демультиплексирование тривиально: `crypto.open` + проверка `CTRL_MAGIC`
//!   в расшифрованном payload отделяет control от Ethernet-фреймов, не трогая
//!   wire-формат датаплейна и не полагаясь на хрупкий разбор первого байта.
//!
//! ## Зачем keepalive ~15-25с
//!
//! NAT выкидывают простаивающие UDP-маппинги (часто через 30-60с). Периодический
//! пакет (дефолт 20с — заведомо ниже типичного порога с запасом) держит маппинг
//! живым, чтобы пир не пропал в тишине простоя.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::crypto::Crypto;

/// Магия в расшифрованном payload, помечающая control-пакет. 8 байт →
/// вероятность совпасть с началом настоящего Ethernet-фрейма ничтожна (~2^-64),
/// а сам пакет ещё и аутентифицирован AEAD.
const CTRL_MAGIC: [u8; 8] = *b"LAT-CTL\x01";

/// Тип control-пакета (один байт после магии).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlKind {
    /// Пробивающий пинг при punch.
    PunchPing,
    /// Ответ на пинг — подтверждает двунаправленный путь.
    PunchPong,
    /// Периодический keepalive в установленной сессии.
    Keepalive,
}

impl CtrlKind {
    fn tag(self) -> u8 {
        match self {
            CtrlKind::PunchPing => 1,
            CtrlKind::PunchPong => 2,
            CtrlKind::Keepalive => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(CtrlKind::PunchPing),
            2 => Some(CtrlKind::PunchPong),
            3 => Some(CtrlKind::Keepalive),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum PunchError {
    #[error("hole punching timed out after {0:?} without a reply from peer")]
    Timeout(Duration),
    #[error("punching aborted (shutdown requested)")]
    Aborted,
    #[error("crypto seal failed during punch: {0}")]
    Crypto(String),
    #[error("punch socket I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Явная машина состояний punch. Переходы прокомментированы «почему».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchState {
    /// Шлём bursts и ждём встречный control-пакет.
    Punching,
    /// Получили ctrl от пира → путь открыт в обе стороны (мы слали, он дошёл).
    Established(SocketAddr),
    /// Истёк таймаут без ответа → вызывающий уходит в relay.
    TimedOut,
    /// Прерывание по shutdown.
    Aborted,
}

/// Параметры punch. Несколько попыток с потолком по времени — не вечный цикл.
#[derive(Debug, Clone, Copy)]
pub struct PunchConfig {
    /// Общий потолок: после него — relay. 5с достаточно для cone-NAT и не
    /// заставляет долго ждать впустую при провале.
    pub total_timeout: Duration,
    /// Интервал между bursts (он же таймаут ожидания ответа на каждую итерацию).
    pub send_interval: Duration,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self {
            total_timeout: Duration::from_secs(5),
            send_interval: Duration::from_millis(200),
        }
    }
}

/// Дефолтный интервал keepalive: 20с — ниже типичного NAT-таймаута (30-60с) с
/// запасом, см. модульный комментарий.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(20);

/// Запечатать control-пакет заданного типа.
///
/// # Errors
///
/// `Crypto` — отказ AEAD при seal (на корректной системе не случается).
pub fn seal_ctrl(crypto: &Crypto, kind: CtrlKind) -> Result<Vec<u8>, PunchError> {
    let mut plain = [0u8; CTRL_MAGIC.len() + 1];
    plain[..CTRL_MAGIC.len()].copy_from_slice(&CTRL_MAGIC);
    plain[CTRL_MAGIC.len()] = kind.tag();
    crypto
        .seal(&plain)
        .map_err(|e| PunchError::Crypto(e.to_string()))
}

/// Распознать тип control-пакета по РАСШИФРОВАННОМУ payload. `None` — это
/// обычный Ethernet-фрейм (не control), его надо писать в TAP. Вызывается из
/// цикла приёма датаплейна для демультиплексирования.
#[must_use]
pub fn control_kind(plaintext: &[u8]) -> Option<CtrlKind> {
    if plaintext.len() < CTRL_MAGIC.len() + 1 || plaintext[..CTRL_MAGIC.len()] != CTRL_MAGIC {
        return None;
    }
    CtrlKind::from_tag(plaintext[CTRL_MAGIC.len()])
}

/// Пробить путь к пиру. Возвращает подтверждённый адрес пира (он может
/// отличаться от анонсированного srflx — берём реальный source ответа).
///
/// На входящий ping отвечаем pong (помогаем пиру подтвердиться), на любой
/// валидный ctrl от пира считаем путь установленным: мы уже слали исходящие
/// (наш NAT открыт), а раз пришёл аутентифицированный ctrl — входящий тоже жив.
///
/// # Errors
///
/// `Timeout` — пир не ответил за `total_timeout`; `Aborted` — запрошен
/// shutdown; `Crypto`/`Io` — отказ seal или сокета.
pub fn punch(
    socket: &UdpSocket,
    crypto: &Crypto,
    peer: SocketAddr,
    cfg: &PunchConfig,
    shutdown: &AtomicBool,
) -> Result<SocketAddr, PunchError> {
    let outbound_ping = seal_ctrl(crypto, CtrlKind::PunchPing)?;
    let reply_pong = seal_ctrl(crypto, CtrlKind::PunchPong)?;

    // На время punch ставим короткий read-timeout, чтобы чередовать send/recv;
    // прежний таймаут датаплейна (200мс) восстановим на выходе.
    let prev_timeout = socket.read_timeout().ok().flatten();
    socket.set_read_timeout(Some(cfg.send_interval))?;

    let deadline = Instant::now() + cfg.total_timeout;
    let mut buf = [0u8; 2048];
    let mut state = PunchState::Punching;

    while Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) {
            state = PunchState::Aborted;
            break;
        }
        // Burst: шлём ping на анонсированный endpoint пира.
        if let Err(e) = socket.send_to(&outbound_ping, peer) {
            log::warn!("punch: send ping to {peer} failed: {e}");
        }

        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                // Только аутентифицированный ctrl значим; чужой/битый пакет
                // open() отсечёт по AEAD-тегу.
                if let Some(plain) = crypto.open(&buf[..n]) {
                    if let Some(kind) = control_kind(&plain) {
                        if kind == CtrlKind::PunchPing {
                            // Пир пробивается к нам — отвечаем pong, чтобы и он
                            // увидел подтверждение.
                            let _ = socket.send_to(&reply_pong, from);
                        }
                        state = PunchState::Established(from);
                        break;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                let _ = socket.set_read_timeout(prev_timeout);
                return Err(PunchError::Io(e));
            }
        }
    }
    if matches!(state, PunchState::Punching) {
        state = PunchState::TimedOut;
    }

    let _ = socket.set_read_timeout(prev_timeout);
    match state {
        PunchState::Established(addr) => {
            log::info!("punch succeeded: direct path to {addr}");
            Ok(addr)
        }
        PunchState::Aborted => Err(PunchError::Aborted),
        _ => Err(PunchError::Timeout(cfg.total_timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Crypto, Key, KEY_LEN};

    fn crypto() -> Crypto {
        Crypto::new(&Key::new([0x5A; KEY_LEN]))
    }

    #[test]
    fn ctrl_roundtrip_each_kind() {
        let c = crypto();
        for kind in [CtrlKind::PunchPing, CtrlKind::PunchPong, CtrlKind::Keepalive] {
            let sealed = seal_ctrl(&c, kind).expect("seal");
            let plain = c.open(&sealed).expect("open");
            assert_eq!(control_kind(&plain), Some(kind));
        }
    }

    #[test]
    fn ethernet_frame_is_not_control() {
        // Случайный «фрейм» без магии → не control, пойдёт в TAP.
        assert_eq!(control_kind(b"\xff\xff\xff\xff\xff\xff......"), None);
        assert_eq!(control_kind(&[]), None);
    }

    #[test]
    fn truncated_magic_is_not_control() {
        assert_eq!(control_kind(&CTRL_MAGIC[..4]), None);
    }
}
