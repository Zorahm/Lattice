//! Установление соединения Фазы 2: STUN → rendezvous → punch / relay.
//!
//! Связывает `stun`, `signaling` и `punch` в одну последовательность и выдаёт
//! `Established` — как датаплейн пойдёт дальше (direct или relay). Discovery
//! Фазы 1 (`--peer`) этот путь не трогает: статика остаётся отдельной веткой в
//! `main` (контракт «не ломать Фазу 1»).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

use lattice_proto::{ClientMessage, NatType, RoomId, ServerMessage, StartMode};

use crate::crypto::Crypto;
use crate::punch::{self, PunchConfig, PunchError};
use crate::signaling::{SignalError, SignalRecv, SignalingClient};
use crate::stun;
use crate::transport::{Transport, UdpTransport};

/// Параметры динамического режима (из CLI).
pub struct DynamicParams {
    pub rendezvous: String,
    pub room: RoomId,
    /// STUN-таргеты (≥2 для эвристики symmetric vs cone). Уже разрешены в `SocketAddr`.
    pub stun_servers: Vec<SocketAddr>,
    pub connect_timeout: Duration,
    /// Сколько ждать второго пира (`Start`) после регистрации.
    pub match_timeout: Duration,
    pub stun_timeout: Duration,
    pub punch: PunchConfig,
}

/// Как пойдёт датаплейн по итогу установления.
#[derive(Debug, Clone, Copy)]
pub enum Established {
    /// Прямой путь пробит: слать данные напрямую на этот адрес пира.
    Direct { peer: SocketAddr },
    /// Punch не сложился: ретрансляция через relay-сокет сервера.
    Relay { server: SocketAddr, session: u64 },
}

#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("signaling failure: {0}")]
    Signal(#[from] SignalError),
    #[error("rendezvous rejected registration: {0}")]
    Rejected(String),
    #[error("control channel closed before peer was matched")]
    ControlLost,
    #[error("timed out waiting {0:?} for a peer in the room")]
    NoPeer(Duration),
    #[error("server sent unparseable address '{0}'")]
    BadAddr(String),
    #[error("aborted (shutdown) during connection setup")]
    Aborted,
}

/// Прогнать STUN → регистрацию → матч → punch/relay. Возвращает решение +
/// живой `SignalingClient` (он нужен сессии для детекта `PeerGone`).
///
/// # Errors
///
/// См. `EstablishError`: недоступный/отклонивший сервер, обрыв control,
/// отсутствие пира за таймаут, битый адрес, shutdown.
pub fn establish(
    transport: &UdpTransport,
    crypto: &Crypto,
    params: &DynamicParams,
    shutdown: &AtomicBool,
) -> Result<(Established, SignalingClient), EstablishError> {
    // 1. STUN на датаплейн-сокете (тот же маппинг, что увидит пир).
    let (srflx, nat) = discover_srflx(transport, params);
    log::info!("local srflx {srflx}, NAT heuristic: {nat:?}");

    // 2. Подключаемся к rendezvous и регистрируемся.
    let mut signaling = SignalingClient::connect(&params.rendezvous, params.connect_timeout)?;
    signaling.register(params.room.clone(), &srflx, nat)?;
    log::info!("registered in room '{}', awaiting peer", params.room.as_str());

    // 3. Ждём go-сигнал (Start) с endpoint'ом пира.
    let start = wait_for_start(&signaling, params.match_timeout, shutdown)?;
    let StartInfo {
        peer,
        mode,
        relay_server,
        session,
    } = start;
    log::info!("peer matched: {peer}, mode {mode:?}, relay {relay_server} (session {session})");

    // 4. Punch или сразу relay (решение сервера по NAT обоих).
    match mode {
        StartMode::Relay => {
            log::info!("server selected relay (symmetric NAT in the pair)");
            Ok((Established::Relay { server: relay_server, session }, signaling))
        }
        StartMode::Punch => {
            match punch::punch(transport.socket(), crypto, peer, &params.punch, shutdown) {
                Ok(addr) => {
                    let _ = signaling.send(&ClientMessage::PunchOk);
                    Ok((Established::Direct { peer: addr }, signaling))
                }
                Err(PunchError::Aborted) => Err(EstablishError::Aborted),
                Err(e) => {
                    // Punch не сошёлся за таймаут → relay. Логируем причину,
                    // сообщаем серверу (для его логов) и переключаемся.
                    log::warn!("punch failed ({e}); falling back to relay");
                    let _ = signaling.send(&ClientMessage::PunchFailed);
                    Ok((Established::Relay { server: relay_server, session }, signaling))
                }
            }
        }
    }
}

/// STUN с деградацией. При полном провале STUN punch невозможен (не знаем
/// srflx) → помечаем NAT как `Symmetric`, чтобы сервер выбрал relay, а не гонял
/// обречённый punch. srflx тогда — локальный адрес (для relay он не важен:
/// сервер узнаёт реальный адрес из source hello-пакета).
fn discover_srflx(transport: &UdpTransport, params: &DynamicParams) -> (String, NatType) {
    match stun::discover(transport.socket(), &params.stun_servers, params.stun_timeout) {
        Ok(o) => (o.srflx.to_string(), o.nat),
        Err(e) => {
            log::warn!("STUN failed ({e}); degrading to relay-only");
            let local = transport
                .local_addr()
                .map_or_else(|_| "0.0.0.0:0".to_string(), |a| a.to_string());
            (local, NatType::Symmetric)
        }
    }
}

struct StartInfo {
    peer: SocketAddr,
    mode: StartMode,
    relay_server: SocketAddr,
    session: u64,
}

/// Дождаться `Start`, переводя строковые адреса в `SocketAddr`. Прочие события
/// (`Registered`, лишние) — логируем и продолжаем ждать в пределах таймаута.
fn wait_for_start(
    signaling: &SignalingClient,
    match_timeout: Duration,
    shutdown: &AtomicBool,
) -> Result<StartInfo, EstablishError> {
    let deadline = Instant::now() + match_timeout;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err(EstablishError::Aborted);
        }
        if Instant::now() >= deadline {
            return Err(EstablishError::NoPeer(match_timeout));
        }
        match signaling.recv(Duration::from_millis(500)) {
            SignalRecv::Message(ServerMessage::Start {
                peer_endpoint,
                mode,
                relay_addr,
                session,
                ..
            }) => {
                let peer = parse_addr(&peer_endpoint)?;
                let relay_server = parse_addr(&relay_addr)?;
                return Ok(StartInfo {
                    peer,
                    mode,
                    relay_server,
                    session,
                });
            }
            SignalRecv::Message(ServerMessage::Registered) => {
                log::debug!("registration acknowledged");
            }
            SignalRecv::Message(ServerMessage::Error { message }) => {
                return Err(EstablishError::Rejected(message));
            }
            SignalRecv::Message(ServerMessage::PeerGone) => {
                log::debug!("stray PeerGone before match, ignoring");
            }
            SignalRecv::Timeout => {}
            SignalRecv::Closed => return Err(EstablishError::ControlLost),
        }
    }
}

fn parse_addr(s: &str) -> Result<SocketAddr, EstablishError> {
    s.parse().map_err(|_| EstablishError::BadAddr(s.to_string()))
}
