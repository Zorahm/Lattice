//! Интеграционные тесты rendezvous + relay. Гоняют реальный сервер in-process
//! на эфемерных портах и два TCP/UDP «клиента» — проверяют матч пиров, выбор
//! режима, ретрансляцию relay и teardown. Живой NAT здесь не нужен: это вся
//! кроссплатформенная логика сервера, которую агент может воспроизвести.

use std::net::{TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

use lattice_proto::{relay as relay_wire, ClientMessage, NatType, RoomId, ServerMessage, StartMode};
use lattice_server::control;
use lattice_server::relay::{self, RelayTable};
use lattice_server::registry::InMemoryRegistry;
use lattice_server::rooms::Rooms;
use lattice_server::wire::{read_frame, write_frame};

/// Поднять сервер на 127.0.0.1:0/0 и вернуть (control_addr, relay_advertise).
/// `registry` для mesh-режима создаётся, но в этих (room) тестах не нужен —
/// `control::serve` принимает его для dispatch room/mesh на одном листенере.
fn spawn_server() -> (String, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind control");
    let control_addr = listener.local_addr().expect("control addr").to_string();
    let relay_socket = UdpSocket::bind("127.0.0.1:0").expect("bind relay");
    let relay_addr = relay_socket.local_addr().expect("relay addr").to_string();

    let table = RelayTable::new();
    let rooms = Rooms::new(table.clone(), relay_addr.clone());
    let registry = InMemoryRegistry::new(table.clone(), relay_addr.clone());

    let relay_table = table;
    thread::spawn(move || relay::serve(&relay_socket, &relay_table));
    thread::spawn(move || control::serve(&listener, &rooms, &registry));

    (control_addr, relay_addr)
}

fn send(stream: &mut TcpStream, msg: &ClientMessage) {
    let json = serde_json::to_vec(msg).expect("serialize");
    write_frame(stream, &json).expect("write frame");
}

fn recv(stream: &mut TcpStream) -> ServerMessage {
    let bytes = read_frame(stream).expect("read frame").expect("not eof");
    serde_json::from_slice(&bytes).expect("deserialize")
}

fn register(addr: &str, room: &str, srflx: &str, nat: NatType) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect control");
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    send(
        &mut s,
        &ClientMessage::Register {
            protocol_version: lattice_proto::PROTOCOL_VERSION,
            room: RoomId::new(room.to_string()),
            srflx: srflx.to_string(),
            nat,
        },
    );
    assert!(matches!(recv(&mut s), ServerMessage::Registered));
    s
}

#[test]
fn matches_two_cone_peers_and_picks_punch() {
    let (control_addr, _relay) = spawn_server();
    let mut a = register(&control_addr, "room-cone", "1.1.1.1:1000", NatType::EndpointIndependent);
    let mut b = register(&control_addr, "room-cone", "2.2.2.2:2000", NatType::EndpointIndependent);

    let start_a = recv(&mut a);
    let start_b = recv(&mut b);

    match start_a {
        ServerMessage::Start { peer_endpoint, mode, .. } => {
            assert_eq!(peer_endpoint, "2.2.2.2:2000"); // A видит endpoint B
            assert_eq!(mode, StartMode::Punch);
        }
        other => panic!("expected Start for A, got {other:?}"),
    }
    match start_b {
        ServerMessage::Start { peer_endpoint, mode, .. } => {
            assert_eq!(peer_endpoint, "1.1.1.1:1000"); // B видит endpoint A
            assert_eq!(mode, StartMode::Punch);
        }
        other => panic!("expected Start for B, got {other:?}"),
    }
}

#[test]
fn symmetric_peer_forces_relay_mode() {
    let (control_addr, _relay) = spawn_server();
    let mut a = register(&control_addr, "room-sym", "1.1.1.1:1000", NatType::Symmetric);
    let mut b = register(&control_addr, "room-sym", "2.2.2.2:2000", NatType::EndpointIndependent);

    for s in [&mut a, &mut b] {
        match recv(s) {
            ServerMessage::Start { mode, .. } => assert_eq!(mode, StartMode::Relay),
            other => panic!("expected Start, got {other:?}"),
        }
    }
}

#[test]
fn third_peer_rejected() {
    let (control_addr, _relay) = spawn_server();
    let _a = register(&control_addr, "room-full", "1.1.1.1:1", NatType::Unknown);
    let _b = register(&control_addr, "room-full", "2.2.2.2:2", NatType::Unknown);

    let mut c = TcpStream::connect(&control_addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(5))).ok();
    send(
        &mut c,
        &ClientMessage::Register {
            protocol_version: lattice_proto::PROTOCOL_VERSION,
            room: RoomId::new("room-full".to_string()),
            srflx: "3.3.3.3:3".to_string(),
            nat: NatType::Unknown,
        },
    );
    assert!(matches!(recv(&mut c), ServerMessage::Error { .. }));
}

#[test]
fn version_mismatch_rejected() {
    let (control_addr, _relay) = spawn_server();
    let mut s = TcpStream::connect(&control_addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    send(
        &mut s,
        &ClientMessage::Register {
            protocol_version: lattice_proto::PROTOCOL_VERSION + 99,
            room: RoomId::new("v".to_string()),
            srflx: "1.1.1.1:1".to_string(),
            nat: NatType::Unknown,
        },
    );
    assert!(matches!(recv(&mut s), ServerMessage::Error { .. }));
}

#[test]
fn peer_gone_on_disconnect() {
    let (control_addr, _relay) = spawn_server();
    let a = register(&control_addr, "room-gone", "1.1.1.1:1", NatType::Unknown);
    let mut b = register(&control_addr, "room-gone", "2.2.2.2:2", NatType::Unknown);
    // оба получают Start
    let _ = recv(&mut b);
    // A уходит → B должен получить PeerGone
    drop(a);
    assert!(matches!(recv(&mut b), ServerMessage::PeerGone));
}

#[test]
fn relay_forwards_ciphertext_between_peers() {
    let (control_addr, relay_addr) = spawn_server();
    let mut a = register(&control_addr, "room-relay", "1.1.1.1:1", NatType::Symmetric);
    let mut b = register(&control_addr, "room-relay", "2.2.2.2:2", NatType::Symmetric);

    let session = match recv(&mut a) {
        ServerMessage::Start { session, mode, .. } => {
            assert_eq!(mode, StartMode::Relay);
            session
        }
        other => panic!("expected Start, got {other:?}"),
    };
    let _ = recv(&mut b); // B's Start

    // Датаплейн-сокеты пиров.
    let sock_a = UdpSocket::bind("127.0.0.1:0").expect("bind a");
    let sock_b = UdpSocket::bind("127.0.0.1:0").expect("bind b");
    sock_b.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Hello от обоих, чтобы сервер узнал их адреса до пересылки данных.
    sock_a.send_to(&relay_wire::encode(session, &[]), &relay_addr).expect("hello a");
    sock_b.send_to(&relay_wire::encode(session, &[]), &relay_addr).expect("hello b");
    thread::sleep(Duration::from_millis(100));

    // A шлёт «ciphertext» → сервер ретранслирует B.
    let ciphertext = b"\x01\x02 nonce+aead payload (server never decrypts)";
    sock_a.send_to(&relay_wire::encode(session, ciphertext), &relay_addr).expect("send a");

    let mut buf = [0u8; 2048];
    let (n, _from) = sock_b.recv_from(&mut buf).expect("b receives relayed");
    let (got_session, payload) = relay_wire::decode(&buf[..n]).expect("decode relayed");
    assert_eq!(got_session, session);
    assert_eq!(payload, ciphertext);
}
