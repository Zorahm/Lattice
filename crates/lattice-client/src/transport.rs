//! Транспортный слой датаплейна. Фаза 1 — голый UDP.
//!
//! Контракт (AGENTS.md «Сменяемый транспорт»): всё, что гонит зашифрованные
//! датаграммы между пирами, стоит за `trait Transport`. Crypto и TAP про
//! транспорт не знают — поэтому Фаза 4 подменит UDP на QUIC (`quinn`, выглядит
//! как HTTP/3) без переписывания crypto/tap. То же для discovery (`trait
//! Discovery` в `peers.rs`).
//!
//! Сигнатура намеренно синхронная (`std::UdpSocket`): для `PoC` два std-потока
//! дешевле рантайма, а `recv` с таймаутом позволяет потоку опрашивать флаг
//! shutdown между попытками — см. `main.rs`.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use thiserror::Error;

// Фаза 4: маскирующие транспорты за тем же `trait Transport`. QUIC — только в
// клиенте (Windows-only уже тянет windows-sys; сервер остаётся без tokio/quinn,
// контракт SPEC соблюдён). Подмодули, а не распухший trait: каждый — своя
// ответственность (≤300 строк).
pub mod obfs;
pub mod quic;
pub mod quic_tls;
pub mod selector;

/// Эффективный MTU датаплейна при QUIC-транспорте: базовый TAP MTU минус оверхед
/// QUIC+DATAGRAM-заголовков. Чтобы не словить тихую фрагментацию/дроп крупных
/// фреймов (QUIC поверх UDP отъедает поверх и так тесного 1380). При голом UDP
/// этот вычет не нужен — QUIC его добавляет только когда активен.
#[must_use]
pub fn quic_effective_mtu(base_mtu: usize) -> usize {
    base_mtu.saturating_sub(quic::QUIC_DATAGRAM_OVERHEAD)
}

/// Максимальный размер буфера приёма. UDP-датаграмма физически не превышает
/// 64KiB, но наш полезный размер после инкапсуляции ≤ MTU + nonce + tag
/// (≈ 1408). Берём с запасом, чтобы чужие негабаритные датаграммы读到 целиком
/// и дропались по длине в crypto, а не обрезались транспортником.
pub const RECV_BUF_LEN: usize = 65_535;

/// Ошибка транспорта. Выделена от `io::Error` чтобы downstream мог матчить
/// «would block» (таймаут — штатный сигнал потоку проверить shutdown) отдельно
/// от реальных ошибок сокета.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport I/O error: {0}")]
    Io(#[from] io::Error),
    /// Сокет отпустил бы управление по таймауту — не ошибка, а сигнал циклу
    /// перефорсировать флаг shutdown и повторить.
    #[error("recv would block / timed out")]
    WouldBlock,
}

/// Сменяемый транспорт датаплейна. Фаза 1: `UdpTransport`. Фаза 4: QUIC-реализация.
pub trait Transport {
    /// Отправить датаграмму пиру. Недостижимость пира (ICMP / no route)
    /// возвращается как `Err` — вызывающий логирует, не падает.
    ///
    /// # Errors
    ///
    /// `Io` — ошибка сокета при `send_to`.
    fn send(&self, addr: SocketAddr, data: &[u8]) -> Result<(), TransportError>;

    /// Принять одну датаграмму. `WouldBlock` означает «таймаут, данных нет» —
    /// цикл приёма использует это чтобы переодически проверять shutdown.
    ///
    /// # Errors
    ///
    /// `WouldBlock` — таймаут (штатный poll); `Io` — прочая ошибка сокета.
    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError>;

    /// Локальный адрес, на котором слушаем — нужен для диагностики в логе.
    ///
    /// # Errors
    ///
    /// Пробрасывает `io::Error` из `UdpSocket::local_addr`.
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

// Blanket-impl'ы, чтобы Фаза 4 композировала транспорты без лишнего владения:
// `&T` — обернуть заимствованный транспорт (напр. `ObfsTransport::new(&udp,..)`,
// когда сам сокет ещё нужен для STUN/punch); `Box<T>` — вернуть выбранный
// транспорт за `dyn` из билдера, не дублируя ветки вызова сессии.
impl<T: Transport + ?Sized> Transport for &T {
    fn send(&self, addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        (**self).send(addr, data)
    }
    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        (**self).recv(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        (**self).local_addr()
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send(&self, addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        (**self).send(addr, data)
    }
    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        (**self).recv(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        (**self).local_addr()
    }
}

/// UDP-реализация `Transport`. Один сокет, bind на старте, неблокирующая
/// настройка не требуется — используем `read_timeout` для возможности
/// graceful shutdown из отдельного потока.
pub struct UdpTransport {
    sock: UdpSocket,
}

impl UdpTransport {
    /// Создать и забиндить UDP-сокет. `read_timeout` делается здесь, чтобы
    /// `recv` не блокировал поток бесконечно и мог опрашивать shutdown.
    ///
    /// # Errors
    ///
    /// `Io` — `UdpSocket::bind` или `set_read_timeout` провалились.
    pub fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let sock = UdpSocket::bind(addr)?;
        // 200мс — компромисс между отзывчивостью shutdown и накладными на
        // повторные recv. Не критично для пропускной PoC.
        sock.set_read_timeout(Some(std::time::Duration::from_millis(200)))?;
        Ok(Self { sock })
    }

    /// Доступ к нижележащему сокету. Нужен Фазе 2: STUN и hole punching обязаны
    /// идти через ТОТ ЖЕ сокет, что и датаплейн (иначе внешний NAT-маппинг не
    /// совпадёт — см. `stun.rs`). Возвращаем `&UdpSocket`, чтобы установочная
    /// фаза переиспользовала сокет, а не плодила новый порт.
    #[must_use]
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }
}

impl Transport for UdpTransport {
    fn send(&self, addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        self.sock.send_to(data, addr)?;
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        match self.sock.recv_from(buf) {
            Ok(pair) => Ok(pair),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
            {
                Err(TransportError::WouldBlock)
            }
            Err(e) => Err(TransportError::Io(e)),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}
