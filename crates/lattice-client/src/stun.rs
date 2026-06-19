//! STUN-клиент (RFC 5389 Binding Request) + эвристика типа NAT.
//!
//! Зачем STUN: узнать свой внешний (srflx) endpoint, чтобы пир знал, куда слать
//! punch-пакеты.
//!
//! **Критично:** STUN делается на ТОМ ЖЕ UDP-сокете, что и датаплейн/punch —
//! внешний маппинг NAT привязан к локальному (ip:port) сокета. На другом сокете
//! srflx не совпал бы с маппингом данных, и punch промахнулся бы. Поэтому
//! функции берут `&UdpSocket` датаплейна.
//!
//! Эвристика NAT: Binding Request на ДВА разных таргета с одного сокета.
//! Одинаковый внешний порт → маппинг не зависит от назначения (cone), punch
//! реален. Разный → симметричный NAT (порт на каждый destination), srflx
//! бесполезен пиру → сразу relay.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use thiserror::Error;

pub use lattice_proto::NatType;

/// STUN magic cookie (RFC 5389): фиксированные 4 байта в заголовке. Им же
/// XOR'ится адрес в `XOR-MAPPED-ADDRESS`.
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Binding Request — тип сообщения (класс request, метод binding).
const BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response.
const BINDING_SUCCESS: u16 = 0x0101;
/// Атрибут `MAPPED-ADDRESS` (legacy, plaintext адрес).
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// Атрибут `XOR-MAPPED-ADDRESS` (современный, адрес XOR'нут magic cookie).
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Длина STUN-заголовка: type(2) + length(2) + cookie(4) + txid(12).
const HEADER_LEN: usize = 20;

/// Внешний (srflx) endpoint, каким нас видит STUN-сервер. Newtype, чтобы не
/// путать с локальным/пировым `SocketAddr` на уровне типа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint(SocketAddr);

impl Endpoint {
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.0
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Error)]
pub enum StunError {
    #[error("STUN I/O error to {server}: {source}")]
    Io {
        server: SocketAddr,
        source: std::io::Error,
    },
    #[error("STUN server {0} did not respond within timeout")]
    Timeout(SocketAddr),
    #[error("malformed STUN response from {server}: {reason}")]
    Malformed {
        server: SocketAddr,
        reason: &'static str,
    },
    #[error("no STUN server responded; cannot determine external endpoint")]
    AllFailed,
}

/// Результат discovery: внешний endpoint + выведенный тип NAT.
#[derive(Debug, Clone, Copy)]
pub struct StunOutcome {
    pub srflx: Endpoint,
    pub nat: NatType,
}

/// Один Binding Request с ретраями. Возвращает srflx, увиденный `server`.
///
/// Ретрансмит нужен, потому что UDP теряет пакеты, а STUN — request/response без
/// собственной надёжности: молчание = либо потеря, либо сервер недоступен.
///
/// # Errors
///
/// `Timeout` — сервер не ответил за `total_timeout`; `Io` — ошибка сокета;
/// `Malformed` — ответ не распарсился (не наш txid / нет адресного атрибута).
pub fn query(
    socket: &UdpSocket,
    server: SocketAddr,
    total_timeout: Duration,
) -> Result<Endpoint, StunError> {
    let mut txid = [0u8; 12];
    OsRng.fill_bytes(&mut txid);
    let request = build_request(&txid);

    let deadline = Instant::now() + total_timeout;
    // Ретрансмит каждые ~250мс. RFC рекомендует экспоненту, но для PoC хватает
    // фиксированного шага — таргеты публичные и отвечают быстро либо не отвечают.
    let mut buf = [0u8; 512];
    while Instant::now() < deadline {
        socket
            .send_to(&request, server)
            .map_err(|source| StunError::Io { server, source })?;

        match recv_matching(socket, server, &txid, &mut buf) {
            Ok(Some(ep)) => return Ok(ep),
            // None = таймаут одной попытки или чужой пакет: ретраим до deadline.
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Err(StunError::Timeout(server))
}

/// Ждать ответ с нашим txid (до 250мс на попытку). `Ok(None)` — ничего/чужое,
/// вызывающий ретраит. Чужие датаграммы (не от STUN-сервера, иной txid) молча
/// пропускаются — на общий датаплейн-сокет может прилетать что угодно.
fn recv_matching(
    socket: &UdpSocket,
    server: SocketAddr,
    txid: &[u8; 12],
    buf: &mut [u8],
) -> Result<Option<Endpoint>, StunError> {
    let prev_timeout = socket.read_timeout().ok().flatten();
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|source| StunError::Io { server, source })?;

    let result = match socket.recv_from(buf) {
        Ok((n, from)) if from == server => parse_response(&buf[..n], txid)
            .map(Some)
            .map_err(|reason| StunError::Malformed { server, reason }),
        Ok(_) => Ok(None), // пакет не от STUN-сервера — игнор.
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            Ok(None) // таймаут попытки.
        }
        Err(source) => Err(StunError::Io { server, source }),
    };

    // Вернуть прежний read_timeout — датаплейн рассчитывает на свой (200мс).
    let _ = socket.set_read_timeout(prev_timeout);
    result
}

/// Собрать Binding Request: заголовок без атрибутов (length=0).
fn build_request(txid: &[u8; 12]) -> [u8; HEADER_LEN] {
    let mut msg = [0u8; HEADER_LEN];
    msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg[2..4].copy_from_slice(&0u16.to_be_bytes()); // length: атрибутов нет.
    msg[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg[8..20].copy_from_slice(txid);
    msg
}

/// Разобрать Binding Success → srflx. Предпочитаем `XOR-MAPPED-ADDRESS`,
/// fallback на legacy `MAPPED-ADDRESS`. Только IPv4 (Фаза 1/2 — IPv4).
fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Result<Endpoint, &'static str> {
    if buf.len() < HEADER_LEN {
        return Err("response shorter than STUN header");
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != BINDING_SUCCESS {
        return Err("not a Binding Success response");
    }
    if buf[8..20] != txid[..] {
        return Err("transaction id mismatch (stale/foreign response)");
    }

    // Атрибуты идут после заголовка: [type:2][len:2][value:len][padding до 4].
    let mut off = HEADER_LEN;
    let mut fallback: Option<Endpoint> = None;
    while off + 4 <= buf.len() {
        let attr_type = u16::from_be_bytes([buf[off], buf[off + 1]]);
        let attr_len = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
        let val_start = off + 4;
        let val_end = val_start + attr_len;
        if val_end > buf.len() {
            break; // обрезанный атрибут — дальше не идём.
        }
        let value = &buf[val_start..val_end];
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(ep) = parse_xor_mapped(value) {
                    return Ok(ep); // предпочтительный — сразу отдаём.
                }
            }
            ATTR_MAPPED_ADDRESS if fallback.is_none() => {
                fallback = parse_mapped(value);
            }
            _ => {}
        }
        // Атрибуты выровнены по 4 байта.
        off = val_start + attr_len.div_ceil(4) * 4;
    }
    fallback.ok_or("no MAPPED-ADDRESS / XOR-MAPPED-ADDRESS attribute")
}

/// `XOR-MAPPED-ADDRESS`: [_:1][family:1][x-port:2][x-addr:4]. Порт XOR старших
/// 16 бит cookie, адрес XOR всего cookie.
fn parse_xor_mapped(v: &[u8]) -> Option<Endpoint> {
    if v.len() < 8 || v[1] != 0x01 {
        return None; // не IPv4.
    }
    let xport = u16::from_be_bytes([v[2], v[3]]);
    let port = xport ^ (MAGIC_COOKIE >> 16) as u16;
    let xaddr = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
    let addr = xaddr ^ MAGIC_COOKIE;
    Some(Endpoint::new(SocketAddr::from((addr.to_be_bytes(), port))))
}

/// Legacy `MAPPED-ADDRESS`: [_:1][family:1][port:2][addr:4], без XOR.
fn parse_mapped(v: &[u8]) -> Option<Endpoint> {
    if v.len() < 8 || v[1] != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]);
    let addr = [v[4], v[5], v[6], v[7]];
    Some(Endpoint::new(SocketAddr::from((addr, port))))
}

/// Полный discovery: srflx + тип NAT по сравнению маппингов на разные таргеты.
///
/// `servers` — упорядоченный список (первый отвечающий даёт srflx). Минимум два
/// разных таргета нужно для вывода symmetric vs cone; с одним вернём `Unknown`.
///
/// # Errors
///
/// `AllFailed` — ни один STUN-сервер не ответил (вызывающий деградирует в relay).
pub fn discover(
    socket: &UdpSocket,
    servers: &[SocketAddr],
    per_server_timeout: Duration,
) -> Result<StunOutcome, StunError> {
    let mut first: Option<Endpoint> = None;
    let mut second: Option<Endpoint> = None;

    for (idx, &server) in servers.iter().enumerate() {
        match query(socket, server, per_server_timeout) {
            Ok(ep) => {
                log::info!("STUN {server} -> srflx {ep}");
                if first.is_none() {
                    first = Some(ep);
                } else {
                    second = Some(ep);
                    break; // двух ответов достаточно для эвристики.
                }
            }
            Err(e) => log::warn!("STUN target #{idx} {server} failed: {e}"),
        }
    }

    match (first, second) {
        (Some(a), Some(b)) => {
            // Разные внешние порты на разные таргеты ⇒ симметричный NAT.
            let nat = if a.addr() == b.addr() {
                NatType::EndpointIndependent
            } else {
                NatType::Symmetric
            };
            Ok(StunOutcome { srflx: a, nat })
        }
        // Один ответ: srflx знаем, тип определить не можем.
        (Some(a), None) => Ok(StunOutcome {
            srflx: a,
            nat: NatType::Unknown,
        }),
        _ => Err(StunError::AllFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(txid: &[u8; 12], attr_type: u16, ip: [u8; 4], port: u16, xor: bool) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes()); // attr header(4) + value(8)
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(txid);
        msg.extend_from_slice(&attr_type.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0);
        msg.push(0x01); // IPv4
        if xor {
            let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
            let xaddr = u32::from_be_bytes(ip) ^ MAGIC_COOKIE;
            msg.extend_from_slice(&xport.to_be_bytes());
            msg.extend_from_slice(&xaddr.to_be_bytes());
        } else {
            msg.extend_from_slice(&port.to_be_bytes());
            msg.extend_from_slice(&ip);
        }
        msg
    }

    #[test]
    fn parse_xor_mapped_address() {
        let txid = [7u8; 12];
        let resp = make_response(&txid, ATTR_XOR_MAPPED_ADDRESS, [203, 0, 113, 5], 51820, true);
        let ep = parse_response(&resp, &txid).expect("parse");
        assert_eq!(ep.addr(), "203.0.113.5:51820".parse().unwrap());
    }

    #[test]
    fn parse_legacy_mapped_address() {
        let txid = [9u8; 12];
        let resp = make_response(&txid, ATTR_MAPPED_ADDRESS, [198, 51, 100, 7], 3478, false);
        let ep = parse_response(&resp, &txid).expect("parse");
        assert_eq!(ep.addr(), "198.51.100.7:3478".parse().unwrap());
    }

    #[test]
    fn rejects_wrong_txid() {
        let txid = [1u8; 12];
        let resp = make_response(&txid, ATTR_XOR_MAPPED_ADDRESS, [1, 2, 3, 4], 1234, true);
        assert!(parse_response(&resp, &[2u8; 12]).is_err());
    }

    #[test]
    fn rejects_non_success() {
        let mut resp = make_response(&[3u8; 12], ATTR_XOR_MAPPED_ADDRESS, [1, 2, 3, 4], 1, true);
        resp[0..2].copy_from_slice(&0x0111u16.to_be_bytes()); // error response
        assert!(parse_response(&resp, &[3u8; 12]).is_err());
    }
}
