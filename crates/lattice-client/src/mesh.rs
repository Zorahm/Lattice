//! Mesh-режим Фазы 3: coordination-сервер сводит N пиров в одной сети.
//!
//! Переиспользует Фазу 2 (`stun`, `punch`, `relay`, `transport`), не ломая
//! static/dynamic. Жизненный цикл:
//! `network-id = BLAKE3(key)` → STUN → `Hello` к coordination-серверу →
//! `Welcome` (список пиров + relay) → punch к каждому пиру → прямой путь или
//! relay-fallback → heartbeat + Presence-апдейты (`PeerJoined`/`PeerLeft`/
//! `PeerUpdated`) → reconnect при обрыве control.
//!
//! ## Транспорт: all-direct или all-relay
//!
//! Per-pair link-статус (`Direct`/`Relay`/`Unknown`) хранится в реестре сервера
//! для `WebUI` (честный «часть напрямую, часть на relay»). Датаплейн-путь в
//! этой архитектуре — бинарный: либо все punch'и удались → `UdpTransport`
//! (напрямую каждому), либо хоть один провалился → `RelayTransport` (всё через
//! relay-сервер, который пересылает каждому кроме отправителя — broadcast-
//! модель TAP-overlay). Смешанный per-pair transport потребовал бы составного
//! транспорта и усложнил бы relay-протокол (сейчас relay-сессия = на сеть);
//! для PoC/MVP бинарный выбор достаточен и сохраняет E2E.
//!
//! ## Overlay-IP и peer-id
//!
//! `overlay-ip` — self-assigned клиентом (из `--tap-ip`), сервер хранит для
//! отображения и детекта коллизий. `peer-id` — генерируется локально
//! (`hostname-pid`), сервер адресует по нему апдейты.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use lattice_proto::mesh::{MeshClientMessage, MeshServerMessage, PeerInfo};
use lattice_proto::{NatType, NetworkId, OverlayIp, PeerId, PROTOCOL_VERSION};

use crate::crypto::Crypto;
use crate::punch::{self, PunchConfig, PunchError};
use crate::relay::RelayTransport;
use crate::stun;
use crate::transport::{Transport, UdpTransport};

use crate::dynamic::Established;

/// Параметры mesh-режима (из CLI).
pub struct MeshParams {
    pub rendezvous: String,
    pub network_id: NetworkId,
    pub peer_id: PeerId,
    pub overlay_ip: OverlayIp,
    /// STUN-таргеты (≥2 для эвристики NAT). Уже разрешены в `SocketAddr`.
    pub stun_servers: Vec<SocketAddr>,
    pub connect_timeout: Duration,
    /// Сколько ждать `Welcome` после `Hello`.
    pub hello_timeout: Duration,
    pub stun_timeout: Duration,
    pub punch: PunchConfig,
    pub heartbeat_interval: Duration,
}

/// Ошибка установления mesh-сессии.
#[derive(Debug, Error)]
pub enum MeshError {
    #[error("signaling failure: {0}")]
    Signal(#[from] MeshSignalError),
    #[error("rendezvous rejected registration: {0}")]
    Rejected(String),
    #[error("control channel closed before Welcome")]
    ControlLost,
    #[error("timed out waiting {0:?} for Welcome")]
    NoWelcome(Duration),
    #[error("server sent unparseable address '{0}'")]
    BadAddr(String),
    #[error("punch aborted (shutdown)")]
    Aborted,
    #[error("relay socket clone failed: {0}")]
    RelayClone(String),
}

/// Ошибка control-канала mesh.
#[derive(Debug, Error)]
pub enum MeshSignalError {
    #[error("cannot resolve rendezvous address '{0}'")]
    Resolve(String),
    #[error("cannot connect to rendezvous {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("control channel I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize control message: {0}")]
    Serialize(String),
}

/// Событие из control-канала mesh.
#[derive(Debug)]
pub enum MeshSignalRecv {
    Message(MeshServerMessage),
    Timeout,
    Closed,
}

/// Control-канал клиента к coordination-серверу (mesh-режим). Аналог
/// `signaling::SignalingClient` Фазы 2, но для `MeshServerMessage`. Пишет в сокет
/// через `Mutex<TcpStream>` — чтобы heartbeat-поток и punch-отчёты могли слать
/// `&self` одновременно (heartbeat каждые 15с, PunchOk/PunchFailed — редко;
/// contention нулевой). Читает через фоновый поток → канал.
pub struct MeshSignaling {
    write: Mutex<TcpStream>,
    events: Mutex<Receiver<MeshServerMessage>>,
}

impl MeshSignaling {
    /// Подключиться к coordination-серверу с таймаутом.
    ///
    /// # Errors
    ///
    /// `Resolve`/`Connect` — адрес не резолвится / соединение не установилось.
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self, MeshSignalError> {
        let resolved = addr
            .to_socket_addrs()
            .map_err(|_| MeshSignalError::Resolve(addr.to_string()))?
            .next()
            .ok_or_else(|| MeshSignalError::Resolve(addr.to_string()))?;
        let stream = TcpStream::connect_timeout(&resolved, timeout).map_err(|source| {
            MeshSignalError::Connect {
                addr: addr.to_string(),
                source,
            }
        })?;
        stream.set_nodelay(true)?;
        let read = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("mesh-signal-reader".into())
            .spawn(move || reader_loop(read, &tx))?;
        Ok(Self {
            write: Mutex::new(stream),
            events: Mutex::new(rx),
        })
    }

    /// Отправить клиентское mesh-сообщение. `&self` — писатель под `Mutex`,
    /// так что heartbeat и punch-отчёты могут звать из разных потоков.
    ///
    /// # Errors
    ///
    /// `Serialize`/`Io` — сбой serde или записи в сокет.
    pub fn send(&self, msg: &MeshClientMessage) -> Result<(), MeshSignalError> {
        let json = serde_json::to_vec(msg).map_err(|e| MeshSignalError::Serialize(e.to_string()))?;
        let mut w = self
            .write
            .lock()
            .map_err(|e| MeshSignalError::Io(std::io::Error::other(e.to_string())))?;
        lattice_proto::framing::write_frame(&mut *w, &json)?;
        Ok(())
    }

    /// Дождаться события сервера до `timeout`.
    #[must_use]
    pub fn recv(&self, timeout: Duration) -> MeshSignalRecv {
        let Ok(rx) = self.events.lock() else {
            return MeshSignalRecv::Closed;
        };
        match rx.recv_timeout(timeout) {
            Ok(msg) => MeshSignalRecv::Message(msg),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => MeshSignalRecv::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => MeshSignalRecv::Closed,
        }
    }
}

fn reader_loop(mut stream: TcpStream, tx: &Sender<MeshServerMessage>) {
    loop {
        match lattice_proto::framing::read_frame(&mut stream) {
            Ok(Some(bytes)) => match serde_json::from_slice::<MeshServerMessage>(&bytes) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(e) => log::warn!("mesh-signal: malformed server message dropped: {e}"),
            },
            Ok(None) => {
                log::info!("mesh-signal: server closed control channel");
                return;
            }
            Err(e) => {
                log::warn!("mesh-signal: control channel read error: {e}");
                return;
            }
        }
    }
}

/// Итог установления mesh: как пойдёт датаплейн + живой signaling.
pub struct MeshEstablished {
    /// `Direct` — все punch'и удались, `UdpTransport` напрямую каждому.
    /// `Relay` — хоть один punch провалился, всё через relay-сервер.
    pub established: Established,
    /// Список пиров сети (обновляется из Presence-апдейтов в `run_mesh`).
    pub peers: Arc<RwLock<Vec<SocketAddr>>>,
    pub signaling: MeshSignaling,
    /// relay-сессия на сеть (для keepalive hello в relay-режиме).
    pub relay_session: u64,
    /// peer-id пиров сети по endpoint — для PunchOk/PunchFailed отчётов и
    /// для идентификации пришедших в `PeerJoined`.
    pub peer_ids: Arc<RwLock<Vec<(PeerId, SocketAddr, OverlayIp)>>>,
    /// Наш публичный IP (из srflx). Пир с тем же публичным IP сидит за тем же
    /// NAT → прямой путь = hairpin (большинство роутеров не умеют) → relay.
    /// Используется сессией для решения по пирам, пришедшим уже после establish.
    pub self_public_ip: Option<IpAddr>,
}

/// Прогнать STUN → `Hello` → `Welcome` → punch-per-peer. Возвращает решение +
/// живой `MeshSignaling` (нужен для heartbeat/Presence-апдейтов в сессии).
///
/// # Errors
///
/// См. `MeshError`: недоступный/отклонивший сервер, обрыв control, отсутствие
/// `Welcome`, битый адрес, shutdown, ошибка clone relay-сокета.
pub fn establish(
    transport: &UdpTransport,
    crypto: &Crypto,
    params: &MeshParams,
    shutdown: &AtomicBool,
) -> Result<MeshEstablished, MeshError> {
    // 1. STUN на датаплейн-сокете (тот же маппинг, что увидят пиры).
    let (srflx, nat) = discover_srflx(transport, params);
    log::info!("mesh: local srflx {srflx}, NAT {nat:?}");
    // Наш публичный IP — для детекта пиров за тем же NAT (hairpin → relay).
    let self_public_ip: Option<IpAddr> = srflx.parse::<SocketAddr>().ok().map(|s| s.ip());

    // 2. Подключаемся к coordination-серверу и шлём Hello.
    let signaling = MeshSignaling::connect(&params.rendezvous, params.connect_timeout)?;
    signaling.send(&MeshClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        network_id: params.network_id.clone(),
        peer_id: params.peer_id.clone(),
        overlay_ip: params.overlay_ip.clone(),
        srflx: srflx.clone(),
        nat,
    })?;
    log::info!(
        "mesh: Hello sent (network {}, peer {}, overlay {})",
        params.network_id.as_str(),
        params.peer_id.as_str(),
        params.overlay_ip.as_str()
    );

    // 3. Ждём Welcome с текущим списком пиров + relay-сессией.
    let welcome = wait_for_welcome(&signaling, params.hello_timeout, shutdown)?;
    log::info!(
        "mesh: Welcome — {} peer(s), relay {}, session {}",
        welcome.peers.len(),
        welcome.relay_addr,
        welcome.session
    );

    // 4. Punch к каждому пиру. Последовательно (N×punch.total_timeout); для
    // PoC приемлемо — N маленький (LAN-overlay). Отчёты уходят серверу для
    // per-pair link-статуса в WebUI.
    let relay_server = parse_addr(&welcome.relay_addr)?;
    let mut direct_peers: Vec<SocketAddr> = Vec::new();
    let mut any_failed = false;
    let mut peer_ids: Vec<(PeerId, SocketAddr, OverlayIp)> = Vec::new();
    for p in &welcome.peers {
        let peer_addr = match parse_addr(&p.srflx) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("mesh: peer {} has bad srflx '{}': {}", p.peer_id.as_str(), p.srflx, e);
                any_failed = true;
                peer_ids.push((p.peer_id.clone(), relay_server, p.overlay_ip.clone()));
                continue;
            }
        };
        peer_ids.push((p.peer_id.clone(), peer_addr, p.overlay_ip.clone()));
        if shutdown.load(Ordering::Acquire) {
            return Err(MeshError::Aborted);
        }
        // Тот же публичный IP, что у нас → пир за нашим же NAT. Прямой путь к
        // его srflx — hairpin, который большинство домашних роутеров не умеют;
        // не тратим 5с на заведомо дохлый punch, сразу relay (бинарная модель:
        // any_failed → вся сеть через relay, что и совпадёт с решением пира).
        if self_public_ip.is_some() && Some(peer_addr.ip()) == self_public_ip {
            log::info!(
                "mesh: peer {} shares our public IP {} (same NAT, hairpin); relay",
                p.peer_id.as_str(),
                peer_addr.ip()
            );
            any_failed = true;
            let _ = signaling.send(&MeshClientMessage::PunchFailed {
                peer_id: p.peer_id.clone(),
            });
            continue;
        }
        match punch::punch(transport.socket(), crypto, peer_addr, &params.punch, shutdown) {
            Ok(addr) => {
                log::info!("mesh: punch to {} ok -> {}", p.peer_id.as_str(), addr);
                direct_peers.push(addr);
                let _ = signaling.send(&MeshClientMessage::PunchOk {
                    peer_id: p.peer_id.clone(),
                });
            }
            Err(PunchError::Aborted) => return Err(MeshError::Aborted),
            Err(e) => {
                log::warn!("mesh: punch to {} failed ({}); relay", p.peer_id.as_str(), e);
                any_failed = true;
                let _ = signaling.send(&MeshClientMessage::PunchFailed {
                    peer_id: p.peer_id.clone(),
                });
            }
        }
    }

    // 5. Выбор транспорта: все-direct или all-relay.
    let established = if any_failed {
        let sock = transport
            .socket()
            .try_clone()
            .map_err(|e| MeshError::RelayClone(e.to_string()))?;
        let relay = RelayTransport::new(sock, relay_server, welcome.session);
        let _ = relay.send_hello();
        Established::Relay {
            server: relay_server,
            session: welcome.session,
        }
    } else {
        Established::Direct {
            peer: direct_peers
                .first()
                .copied()
                .unwrap_or(relay_server),
        }
    };

    // MeshPeers: в direct-режиме — все direct endpoint'ы; в relay — [relay_server]
    // (relay пересылает каждому кроме отправителя, один addr Enough).
    let peers_list: Vec<SocketAddr> = match &established {
        Established::Direct { .. } => direct_peers,
        Established::Relay { server, .. } => vec![*server],
    };
    let peers = Arc::new(RwLock::new(peers_list));

    Ok(MeshEstablished {
        established,
        peers,
        signaling,
        relay_session: welcome.session,
        peer_ids: Arc::new(RwLock::new(peer_ids)),
        self_public_ip,
    })
}

/// STUN с деградацией (как `dynamic::discover_srflx`, но для mesh).
fn discover_srflx(transport: &UdpTransport, params: &MeshParams) -> (String, NatType) {
    match stun::discover(transport.socket(), &params.stun_servers, params.stun_timeout) {
        Ok(o) => (o.srflx.to_string(), o.nat),
        Err(e) => {
            log::warn!("mesh: STUN failed ({e}); degrading to relay-only");
            let local = transport
                .local_addr()
                .map_or_else(|_| "0.0.0.0:0".to_string(), |a| a.to_string());
            (local, NatType::Symmetric)
        }
    }
}

struct WelcomeData {
    peers: Vec<PeerInfo>,
    relay_addr: String,
    session: u64,
}

fn wait_for_welcome(
    signaling: &MeshSignaling,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> Result<WelcomeData, MeshError> {
    let deadline = Instant::now() + timeout;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err(MeshError::Aborted);
        }
        if Instant::now() >= deadline {
            return Err(MeshError::NoWelcome(timeout));
        }
        match signaling.recv(Duration::from_millis(500)) {
            MeshSignalRecv::Message(MeshServerMessage::Welcome {
                peers,
                relay_addr,
                session,
            }) => return Ok(WelcomeData {
                peers,
                relay_addr,
                session,
            }),
            MeshSignalRecv::Message(MeshServerMessage::Error { message }) => {
                return Err(MeshError::Rejected(message));
            }
            MeshSignalRecv::Message(other) => {
                log::debug!("mesh: pre-Welcome, ignoring {other:?}");
            }
            MeshSignalRecv::Timeout => {}
            MeshSignalRecv::Closed => return Err(MeshError::ControlLost),
        }
    }
}

fn parse_addr(s: &str) -> Result<SocketAddr, MeshError> {
    s.parse().map_err(|_| MeshError::BadAddr(s.to_string()))
}
