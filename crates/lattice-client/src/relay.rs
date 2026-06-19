//! Relay-транспорт датаплейна (fallback при провале punch).
//!
//! Реализует тот же `trait Transport`, что и `UdpTransport` — поэтому циклы
//! tap→udp / udp→tap не меняются: меняется лишь реализация транспорта по итогу
//! punch (см. AGENTS.md «Сменяемый транспорт»). Вместо прямой отправки пиру
//! заворачиваем датаграмму в relay-обёртку (`lattice_proto::relay`) и шлём на
//! relay-сокет сервера; приём — снимаем обёртку.
//!
//! **E2E не ослабляется:** в обёртку кладётся уже зашифрованная датаграмма
//! `[nonce || AEAD(frame)]`; сервер видит только её и адреса, ключа не имеет.

use std::net::{SocketAddr, UdpSocket};

use lattice_proto::relay as wire;

use crate::transport::{Transport, TransportError};

/// Транспорт поверх серверного relay. Сокет — `try_clone` датаплейн-сокета (тот
/// же внешний маппинг, что узнал сервер по hello), `session` — выданный при
/// матче идентификатор relay-сессии.
pub struct RelayTransport {
    sock: UdpSocket,
    server: SocketAddr,
    session: u64,
}

impl RelayTransport {
    #[must_use]
    pub fn new(sock: UdpSocket, server: SocketAddr, session: u64) -> Self {
        Self {
            sock,
            server,
            session,
        }
    }

    /// Послать relay-«hello» (пустой payload): сервер по нему запоминает наш
    /// внешний адрес ещё до потока данных — иначе ему некуда слать пакеты пира.
    /// Вызывается при переходе в relay и периодически как keepalive.
    ///
    /// # Errors
    ///
    /// `Io` — отправка на relay-сокет сервера не удалась.
    pub fn send_hello(&self) -> Result<(), TransportError> {
        self.sock.send_to(&wire::encode(self.session, &[]), self.server)?;
        Ok(())
    }
}

impl Transport for RelayTransport {
    /// `addr` игнорируем намеренно: в relay всё идёт на сервер, который сам
    /// решает, кому переслать (по session + source). Сигнатуру держим общей с
    /// `UdpTransport`, чтобы датаплейн-циклы не знали, какой транспорт под ними.
    fn send(&self, _addr: SocketAddr, data: &[u8]) -> Result<(), TransportError> {
        self.sock
            .send_to(&wire::encode(self.session, data), self.server)?;
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), TransportError> {
        match self.sock.recv_from(buf) {
            Ok((n, from)) if from == self.server => {
                // Снимаем relay-обёртку; payload сдвигаем в начало buf, чтобы
                // вернуть его как обычную датаграмму (crypto.open разберёт).
                match wire::decode(&buf[..n]) {
                    Some((_session, payload)) => {
                        let len = payload.len();
                        buf.copy_within(wire::RELAY_HEADER_LEN..n, 0);
                        Ok((len, from))
                    }
                    // Не relay-пакет от сервера (мусор) — как таймаут, повтор.
                    None => Err(TransportError::WouldBlock),
                }
            }
            // Пакет не от relay-сервера (поздний punch / скан) — игнор, повтор.
            Ok(_) => Err(TransportError::WouldBlock),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Err(TransportError::WouldBlock)
            }
            Err(e) => Err(TransportError::Io(e)),
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}
