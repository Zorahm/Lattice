//! Control-канал: TCP-листенер + по потоку на соединение.
//!
//! Фаза 3: листенер обслуживает ДВА режима на одном порту. Первое сообщение
//! соединения решает, room (Фаза 2, `ClientMessage::Register`) или mesh
//! (Фаза 3, `MeshClientMessage::Hello`). dispatch читает первый кадр и
//! направляет в `reader_loop` (room) или `mesh_control::handle_connection`
//! (mesh). Поведение room-path не изменилось — Фаза 2 не сломана.
//!
//! Каждое соединение обслуживают ДВА потока: reader (читает сообщения,
//! регистрирует/снимает участника) и writer (тянет `ServerMessage` из mpsc и
//! пишет в сокет). Разделение нужно, потому что сервер пушит сообщения
//! асинхронно — `Start`/`PeerJoined` приходят, пока клиент молча ждёт; держать
//! на этом блокирующий `recv` нельзя. mpsc развязывает «кто шлёт» от «когда
//! пишется в сокет».

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use lattice_proto::{ClientMessage, MeshClientMessage, RoomId, ServerMessage, PROTOCOL_VERSION};

use crate::mesh_control;
use crate::registry::Registry;
use crate::rooms::{Member, Rooms};
use crate::wire::{read_frame, write_frame};

/// Монотонный id соединения — для логов и идентификации участника при teardown.
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Принимать control-соединения, пока листенер жив. По соединению — отдельный
/// поток; ошибка `accept` логируется и не роняет цикл. `registry` — для mesh;
/// `rooms` — для room (Фаза 2). `R: Clone` — копия на каждый accept-поток.
pub fn serve<R: Registry + Clone + 'static>(listener: &TcpListener, rooms: &Rooms, registry: &R) {
    log::info!(
        "control listening on {} (room + mesh on one port)",
        listener
            .local_addr()
            .map_or_else(|_| "<unknown>".to_string(), |a| a.to_string())
    );
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                let rooms = rooms.clone();
                let registry = registry.clone();
                let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = thread::Builder::new()
                    .name(format!("ctrl-{conn_id}"))
                    .spawn(move || handle_connection(stream, peer, conn_id, &rooms, &registry))
                {
                    log::error!("control: failed to spawn handler: {e}");
                }
            }
            Err(e) => log::warn!("control: accept failed: {e}"),
        }
    }
}

fn handle_connection<R: Registry + 'static>(
    mut stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    rooms: &Rooms,
    registry: &R,
) {
    // Nagle мешает интерактивному go-сигналу (мелкие кадры буферизуются) — off.
    let _ = stream.set_nodelay(true);

    // Читаем первый кадр здесь, чтобы dispatch'нуть room vs mesh. Чужой/битый
    // кадр → комнатный path (он пришлёт `Error` сам), mesh-режим определяется
    // только успешным `MeshClientMessage::Hello`.
    let first = match read_frame(&mut stream) {
        Ok(Some(frame)) => frame,
        Ok(None) => return, // закрыли сразу.
        Err(e) => {
            log::debug!("control[{conn_id}] {peer}: first frame read failed: {e}");
            return;
        }
    };

    if is_mesh_hello(&first) {
        // Mesh path: делегируем в mesh_control — он сам создаст writer/reader.
        // stream уже без первого кадра (мы его прочитали), передаём кадр дальше.
        mesh_control::handle_connection(stream, peer, &first, registry);
        return;
    }

    // Room path (Фаза 2): поведение не изменилось.
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("control[{conn_id}]: try_clone failed: {e}");
            return;
        }
    };
    let (tx, rx): (Sender<ServerMessage>, Receiver<ServerMessage>) = mpsc::channel();
    let writer = thread::Builder::new()
        .name(format!("ctrl-{conn_id}-w"))
        .spawn(move || writer_loop(write_stream, &rx));

    if let Err(e) = reader_loop(stream, conn_id, peer, &first, tx, rooms) {
        log::debug!("control[{conn_id}] {peer}: session ended: {e}");
    }
    if let Ok(handle) = writer {
        let _ = handle.join();
    }
}

/// Является ли первый кадр `MeshClientMessage::Hello`? Пробуем десериализовать
/// как mesh-сообщение;Externally-tagged enum (`{"Hello": {...}}`), так что
/// случайный `ClientMessage::Register` (`{"Register": {...}}`) не совпадёт.
fn is_mesh_hello(frame: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<MeshClientMessage>(frame),
        Ok(MeshClientMessage::Hello { .. })
    )
}

/// Сериализовать и слать `ServerMessage`, пока канал/сокет живы.
fn writer_loop(mut stream: TcpStream, rx: &Receiver<ServerMessage>) {
    while let Ok(msg) = rx.recv() {
        let json = match serde_json::to_vec(&msg) {
            Ok(j) => j,
            Err(e) => {
                log::error!("control: serialize {msg:?} failed: {e}");
                continue;
            }
        };
        if let Err(e) = write_frame(&mut stream, &json) {
            log::debug!("control: write failed, closing writer: {e}");
            return;
        }
    }
}

/// Читать `ClientMessage`, пока соединение живо. Первое сообщение (передано из
/// `handle_connection`) обязано быть `Register`. `tx` отдаём в реестр (внутри
/// `Member`); по выходу он дропается и гасит writer.
fn reader_loop(
    mut stream: TcpStream,
    conn_id: u64,
    peer: SocketAddr,
    first: &[u8],
    tx: Sender<ServerMessage>,
    rooms: &Rooms,
) -> io::Result<()> {
    let Some(reg) = parse_register(first, &tx) else {
        return Ok(()); // версия/формат не подошли — Error уже отправлен.
    };

    let room_id = reg.room.clone();
    // Member забирает tx по значению; writer держит свою копию write-half сокета,
    // не tx, так что гашение writer'а завязано на дроп tx именно здесь.
    let _ = rooms.register(
        &room_id,
        Member {
            conn_id,
            srflx: reg.srflx,
            nat: reg.nat,
            tx,
        },
    );
    log::info!("control[{conn_id}] {peer}: registered in room '{}'", room_id.as_str());

    // Дальнейшие сообщения сессии.
    let result = session_loop(&mut stream, conn_id);
    // Любой выход (EOF, Bye, ошибка) = участник ушёл → teardown комнаты.
    rooms.leave(&room_id, conn_id);
    result
}

/// Разобранный `Register`.
struct RegisterData {
    room: RoomId,
    srflx: String,
    nat: lattice_proto::NatType,
}

/// Цикл после регистрации: punch-отчёты / `Bye` / штатный EOF.
fn session_loop(stream: &mut TcpStream, conn_id: u64) -> io::Result<()> {
    loop {
        let Some(frame) = read_frame(stream)? else {
            return Ok(()); // EOF — пир закрыл соединение.
        };
        match serde_json::from_slice::<ClientMessage>(&frame) {
            Ok(ClientMessage::Bye) => return Ok(()),
            Ok(ClientMessage::PunchFailed) => {
                // relay уже доступен (адрес/сессия были в Start) — клиент уходит
                // туда сам, серверу действий не нужно, только лог причины.
                log::info!("control[{conn_id}]: punch failed, peer falling back to relay");
            }
            Ok(ClientMessage::PunchOk) => {
                log::info!("control[{conn_id}]: direct punch succeeded");
            }
            Ok(ClientMessage::Register { .. }) => {
                log::warn!("control[{conn_id}]: unexpected re-Register, ignored");
            }
            Err(e) => log::warn!("control[{conn_id}]: malformed message dropped: {e}"),
        }
    }
}

/// Распарсить `Register` и провалидировать версию протокола. На несовпадении
/// шлёт `Error` и возвращает `None`.
fn parse_register(frame: &[u8], tx: &Sender<ServerMessage>) -> Option<RegisterData> {
    match serde_json::from_slice::<ClientMessage>(frame) {
        Ok(ClientMessage::Register {
            protocol_version,
            room,
            srflx,
            nat,
        }) => {
            if protocol_version != PROTOCOL_VERSION {
                let _ = tx.send(ServerMessage::Error {
                    message: format!(
                        "protocol version mismatch: server {PROTOCOL_VERSION}, client {protocol_version}"
                    ),
                });
                return None;
            }
            Some(RegisterData { room, srflx, nat })
        }
        Ok(_) => {
            let _ = tx.send(ServerMessage::Error {
                message: "first message must be Register".into(),
            });
            None
        }
        Err(e) => {
            let _ = tx.send(ServerMessage::Error {
                message: format!("malformed Register: {e}"),
            });
            None
        }
    }
}
