//! Mesh control-канал Фазы 3: TCP, по потоку на соединение.
//!
//! Архитектура как в `control` Фазы 2 (reader+writer потока, mpsc развязка),
//! но сообщения — `MeshClientMessage`/`MeshServerMessage`, а состояние —
//! `Registry` (сетей и пиров), не `Rooms` (2-пировых комнат). Сервер
//! обслуживает оба режима на одном control-TCP-листенере: dispatch в `control`
//! читает первый кадр и определяет room vs mesh по типу сообщения.
//!
//! Жизненный цикл соединения:
//! `Hello` → `registry.join` → `Welcome` новичку + `PeerJoined` остальным →
//! heartbeat/`PunchOk`/`PunchFailed` цикл → `Bye`/EOF → `registry.leave` +
//! `PeerLeft` остальным. Переподключение с тем же `peer-id` → `registry.join`
//! обновляет запись, рассылает `PeerUpdated` (не дубль).

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use lattice_proto::mesh::{LinkKind, MeshClientMessage, MeshServerMessage};
use lattice_proto::{NatType, NetworkId, OverlayIp, PeerId, PROTOCOL_VERSION};

use crate::registry::{JoinRequest, Registry, RegistryError};
use crate::wire::{read_frame, write_frame};

/// Обработать mesh-соединение. `first_frame` — уже прочитанный dispatch'ем
/// первый кадр (это обязан `MeshClientMessage::Hello`, иначе dispatch не
/// направил бы сюда). Reader-поток: обрабатывает сообщения, пишет ответы через
/// `tx`; writer-поток: единственный, кто пишет в сокет.
pub fn handle_connection<R: Registry + 'static>(
    stream: TcpStream,
    peer: SocketAddr,
    first_frame: &[u8],
    registry: &R,
) {
    // Nagle off: мелкие control-кадры (`PeerJoined`) не должны буферизоваться.
    let _ = stream.set_nodelay(true);
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("mesh: try_clone failed: {e}");
            return;
        }
    };

    let (tx, rx): (Sender<MeshServerMessage>, Receiver<MeshServerMessage>) = mpsc::channel();
    let writer = thread::Builder::new()
        .name("mesh-w".into())
        .spawn(move || writer_loop(write_stream, &rx));

    let result = reader_loop(stream, peer, first_frame, &tx, registry);
    if let Err(e) = result {
        log::debug!("mesh {peer}: session ended: {e}");
    }
    if let Ok(handle) = writer {
        let _ = handle.join();
    }
}

/// Сериализовать и слать `MeshServerMessage`, пока канал/сокет живы.
fn writer_loop(mut stream: TcpStream, rx: &Receiver<MeshServerMessage>) {
    while let Ok(msg) = rx.recv() {
        let json = match serde_json::to_vec(&msg) {
            Ok(j) => j,
            Err(e) => {
                log::error!("mesh: serialize {msg:?} failed: {e}");
                continue;
            }
        };
        if let Err(e) = write_frame(&mut stream, &json) {
            log::debug!("mesh: write failed, closing writer: {e}");
            return;
        }
    }
}

/// Разобрать `Hello` и зарегистрировать пира. `Err` — протокол нарушен, `tx`
/// уже получил `Error` (клиент закроется).
fn parse_hello(
    frame: &[u8],
    tx: &Sender<MeshServerMessage>,
) -> Option<HelloData> {
    match serde_json::from_slice::<MeshClientMessage>(frame) {
        Ok(MeshClientMessage::Hello {
            protocol_version,
            network_id,
            peer_id,
            overlay_ip,
            srflx,
            nat,
        }) => {
            if protocol_version != PROTOCOL_VERSION {
                let _ = tx.send(MeshServerMessage::Error {
                    message: format!(
                        "protocol version mismatch: server {PROTOCOL_VERSION}, client {protocol_version}"
                    ),
                });
                return None;
            }
            // Дополнительная валидация network-id (на случай, если клиент прислал
            // невалидный хэш — registry не повторяет валидацию, она в newtype).
            if NetworkId::from_hex(network_id.as_str()).is_err() {
                let _ = tx.send(MeshServerMessage::Error {
                    message: "invalid network-id (expected 64 hex chars)".into(),
                });
                return None;
            }
            Some(HelloData {
                network_id,
                peer_id,
                overlay_ip,
                srflx,
                nat,
            })
        }
        Ok(other) => {
            let _ = tx.send(MeshServerMessage::Error {
                message: format!("first mesh message must be Hello, got {other:?}"),
            });
            None
        }
        Err(e) => {
            let _ = tx.send(MeshServerMessage::Error {
                message: format!("malformed Hello: {e}"),
            });
            None
        }
    }
}

struct HelloData {
    network_id: NetworkId,
    peer_id: PeerId,
    overlay_ip: OverlayIp,
    srflx: String,
    nat: NatType,
}

/// Reader-цикл: `Hello` → `join` → сессия (heartbeat/punch-отчёты/`Bye`).
/// Любой выход (EOF, `Bye`, ошибка) = `leave` — корректный teardown сети.
fn reader_loop<R: Registry>(
    mut stream: TcpStream,
    peer: SocketAddr,
    first_frame: &[u8],
    tx: &Sender<MeshServerMessage>,
    registry: &R,
) -> io::Result<()> {
    let Some(hello) = parse_hello(first_frame, tx) else {
        return Ok(()); // Error уже отправлен.
    };
    let HelloData {
        network_id,
        peer_id,
        overlay_ip,
        srflx,
        nat,
    } = hello;

    let welcome = match registry.join(JoinRequest {
        network_id: network_id.clone(),
        peer_id: peer_id.clone(),
        overlay_ip: overlay_ip.clone(),
        srflx: srflx.clone(),
        nat,
        control_addr: peer,
        tx: tx.clone(),
    }) {
        Ok(w) => w,
        Err(RegistryError::OverlayIpCollision(ip)) => {
            let _ = tx.send(MeshServerMessage::Error {
                message: format!("overlay-ip {} already taken in this network", ip.as_str()),
            });
            return Ok(());
        }
        Err(e) => {
            let _ = tx.send(MeshServerMessage::Error {
                message: format!("registration rejected: {e}"),
            });
            return Ok(());
        }
    };

    // Welcome уходит новичку — текущий список сети + relay-сессия.
    let json = serde_json::to_vec(&MeshServerMessage::Welcome {
        peers: welcome.peers.clone(),
        relay_addr: welcome.relay_addr.clone(),
        session: welcome.session,
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    write_frame(&mut stream, &json)?;

    log::info!(
        "mesh {peer}: peer {} joined network {} ({} peers, relay session {})",
        peer_id.as_str(),
        network_id.as_str(),
        welcome.peers.len() + 1,
        welcome.session
    );

    // Сессионный цикл. Серверные пуш (PeerJoined/PeerLeft/PeerUpdated от других)
    // идёт через `tx`, сохранённый в реестре при `join` — registry.broadcast
    // шлёт напрямую в writer-поток. Здесь только читаем входящие.
    let result = session_loop(&mut stream, &network_id, &peer_id, registry);
    // Любой выход = participant ушёл → teardown записи.
    registry.leave(&network_id, &peer_id);
    result
}

/// Цикл после `Hello`: heartbeat'ы, punch-отчёты, `Bye`/EOF. Серверные пуш
/// (`PeerJoined`/`PeerLeft`/`PeerUpdated`) идут в writer через `tx` в реестре,
/// не отсюда — эта функция только читает входящие сообщения клиента.
fn session_loop<R: Registry>(
    stream: &mut TcpStream,
    network_id: &NetworkId,
    peer_id: &PeerId,
    registry: &R,
) -> io::Result<()> {
    loop {
        let Some(frame) = read_frame(stream)? else {
            return Ok(()); // EOF — пир закрыл соединение.
        };
        match serde_json::from_slice::<MeshClientMessage>(&frame) {
            Ok(MeshClientMessage::Heartbeat) => {
                registry.heartbeat(network_id, peer_id);
            }
            Ok(MeshClientMessage::PunchOk { peer_id: to }) => {
                registry.punch_report(network_id, peer_id, &to, LinkKind::Direct);
            }
            Ok(MeshClientMessage::PunchFailed { peer_id: to }) => {
                registry.punch_report(network_id, peer_id, &to, LinkKind::Relay);
            }
            Ok(MeshClientMessage::Bye) => return Ok(()),
            Ok(MeshClientMessage::Hello { .. }) => {
                log::warn!("mesh: unexpected re-Hello from {}, ignored", peer_id.as_str());
            }
            Err(e) => log::warn!("mesh: malformed message dropped: {e}"),
        }
    }
}
