//! Unit-тесты `InMemoryRegistry` — реестр сетей/пиров Фазы 3 без TCP-обвязки.
//! Покрывают: join/Welcome/PeerJoined, leave/PeerLeft, heartbeat, punch_report,
//! overlay-IP коллизию, re-join (обновление), kick, close_network, snapshot,
//! presence_sweep (timeout → PeerLeft).

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use lattice_proto::mesh::{LinkKind, MeshServerMessage};
use lattice_proto::{NatType, NetworkId, OverlayIp, PeerId};

use lattice_server::relay::RelayTable;
use lattice_server::registry::{InMemoryRegistry, JoinRequest, Registry, HEARTBEAT_OFFLINE_AFTER};

// 32 повтора "ab" = 64 hex-символа — валидный `NetworkId`.
const HEX_REPEATS: usize = 32;

fn net_id() -> NetworkId {
    NetworkId::from_hex(&"ab".repeat(HEX_REPEATS)).expect("valid hex")
}

fn net_id_b() -> NetworkId {
    NetworkId::from_hex(&"cd".repeat(HEX_REPEATS)).expect("valid hex")
}

fn peer(name: &str) -> PeerId {
    PeerId::new(name.to_string())
}

fn overlay(ip: &str) -> OverlayIp {
    OverlayIp::new(ip.to_string())
}

fn ctrl_addr() -> SocketAddr {
    "127.0.0.1:1234".parse().expect("addr")
}

/// Зарегистрировать пира с собственным mpsc-каналом; вернуть `(rx, welcome)`.
/// `rx` — приёмник серверных пушей (PeerJoined/PeerLeft/...), которые реестр
/// шлёт через `tx`, сохранённый в записи.
fn join(
    reg: &InMemoryRegistry,
    net_id: NetworkId,
    peer_id: PeerId,
    overlay_ip: OverlayIp,
    srflx: &str,
) -> (mpsc::Receiver<MeshServerMessage>, lattice_server::registry::Welcome) {
    let (tx, rx) = mpsc::channel();
    let w = reg
        .join(JoinRequest {
            network_id: net_id,
            peer_id,
            overlay_ip,
            srflx: srflx.to_string(),
            nat: NatType::EndpointIndependent,
            local_addr: None,
            control_addr: ctrl_addr(),
            tx,
        })
        .expect("join");
    (rx, w)
}

#[test]
fn first_peer_gets_empty_welcome() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "1.2.3.4:9".to_string());
    let (_, w) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1000");
    assert!(w.peers.is_empty());
    assert_eq!(w.relay_addr, "1.2.3.4:9");
    assert!(w.session > 0);
}

#[test]
fn second_peer_gets_first_in_welcome_and_first_gets_peer_joined() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _w_a) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1000");
    let (_rx_b, w_b) = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2000");

    // B видит A в welcome.peers.
    assert_eq!(w_b.peers.len(), 1);
    assert_eq!(w_b.peers[0].peer_id, peer("A"));
    // A получает PeerJoined о B.
    let msg = rx_a.recv_timeout(Duration::from_secs(1)).expect("A got push");
    match msg {
        MeshServerMessage::PeerJoined(info) => assert_eq!(info.peer_id, peer("B")),
        other => panic!("expected PeerJoined, got {other:?}"),
    }
}

#[test]
fn overlay_ip_collision_rejected() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let _ = join(&reg, net_id(), peer("A"), overlay("10.66.0.5"), "1.1.1.1:1");
    let (tx_b, _rx_b) = mpsc::channel();
    let err = reg
        .join(JoinRequest {
            network_id: net_id(),
            peer_id: peer("B"),
            overlay_ip: overlay("10.66.0.5"), // та же
            srflx: "2.2.2.2:2".to_string(),
            nat: NatType::Unknown,
            local_addr: None,
            control_addr: ctrl_addr(),
            tx: tx_b,
        })
        .expect_err("collision should error");
    assert!(matches!(
        err,
        lattice_server::registry::RegistryError::OverlayIpCollision(_)
    ));
}

#[test]
fn rejoin_updates_record_and_broadcasts_peer_updated() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1000");
    let (rx_b, _) = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2000");
    let _ = rx_a.recv_timeout(Duration::from_millis(200)).expect("A got PeerJoined");
    let _ = rx_b.recv_timeout(Duration::from_millis(200)); // B ничего не получает о себе.

    // A переподключается с новым endpoint. Реестр обновляет запись A и шлёт
    // PeerUpdated всем кроме A — т.е. B (через rx_b, сохранённый в записи B).
    let (_rx_a2, w_a2) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "9.9.9.9:9000");
    let msg = rx_b.recv_timeout(Duration::from_secs(1)).expect("B got PeerUpdated");
    match msg {
        MeshServerMessage::PeerUpdated(info) => {
            assert_eq!(info.peer_id, peer("A"));
            assert_eq!(info.srflx, "9.9.9.9:9000");
        }
        other => panic!("expected PeerUpdated, got {other:?}"),
    }
    // A видит B в welcome (сеть не пуста).
    assert_eq!(w_a2.peers.len(), 1);
    assert_eq!(w_a2.peers[0].peer_id, peer("B"));
}

#[test]
fn leave_broadcasts_peer_left_and_removes_empty_network() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    let (_rx_b, _) = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2");
    let _ = rx_a.recv_timeout(Duration::from_millis(200));

    reg.leave(&net_id(), &peer("B"));
    let msg = rx_a.recv_timeout(Duration::from_secs(1)).expect("A got PeerLeft");
    assert!(matches!(msg, MeshServerMessage::PeerLeft { .. }));

    // Сеть ещё есть (A жив).
    assert_eq!(reg.snapshot().len(), 1);
    reg.leave(&net_id(), &peer("A"));
    // Последний ушёл — сеть удалилась.
    assert!(reg.snapshot().is_empty());
}

#[test]
fn punch_report_records_link_in_snapshot() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let _ = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    let _ = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2");
    reg.punch_report(&net_id(), &peer("A"), &peer("B"), LinkKind::Direct);
    let snap = reg.snapshot();
    let a = snap
        .iter()
        .flat_map(|n| n.peers.iter())
        .find(|p| p.peer_id == peer("A"))
        .expect("A in snapshot");
    let link_b = a.links.iter().find(|(p, _)| *p == peer("B")).expect("link to B");
    assert_eq!(link_b.1, LinkKind::Direct);
}

#[test]
fn kick_removes_peer_and_broadcasts() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    let (rx_b, _) = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2");
    let _ = rx_a.recv_timeout(Duration::from_millis(200));

    reg.kick(&net_id(), &peer("B"), "test kick");
    // B получил Kicked.
    let msg_b = rx_b.recv_timeout(Duration::from_secs(1)).expect("B got Kicked");
    assert!(matches!(msg_b, MeshServerMessage::Kicked { .. }));
    // A получил PeerLeft.
    let msg_a = rx_a.recv_timeout(Duration::from_secs(1)).expect("A got PeerLeft");
    assert!(matches!(msg_a, MeshServerMessage::PeerLeft { .. }));
    assert_eq!(reg.snapshot().len(), 1); // A ещё в сети.
}

#[test]
fn close_network_broadcasts_and_removes() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    reg.close_network(&net_id(), "test close");
    let msg = rx_a.recv_timeout(Duration::from_secs(1)).expect("A got NetworkClosed");
    assert!(matches!(msg, MeshServerMessage::NetworkClosed { .. }));
    assert!(reg.snapshot().is_empty());
}

#[test]
fn presence_sweep_removes_stale_peers() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let (rx_a, _) = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    let (rx_b, _) = join(&reg, net_id(), peer("B"), overlay("10.66.0.2"), "2.2.2.2:2");
    let _ = rx_a.recv_timeout(Duration::from_millis(100));

    // A heartbeat'ит, B — нет. Сильно ускорим порог: heartbeat_interval=50ms,
    // offline_after=3 → B протухнет через ~150мс без heartbeat.
    let hb = Duration::from_millis(50);
    // Подождём достаточно, чтобы B точно протух (5 интервалов).
    std::thread::sleep(hb * (HEARTBEAT_OFFLINE_AFTER as u32 + 2));
    // A ещё «жив», поддержим heartbeat.
    reg.heartbeat(&net_id(), &peer("A"));
    let removed = reg.presence_sweep(hb, HEARTBEAT_OFFLINE_AFTER);
    // B должен быть удалён.
    assert!(removed.iter().any(|(_, p)| *p == peer("B")));
    // A получил PeerLeft о B.
    let msg = rx_a.recv_timeout(Duration::from_secs(1)).expect("A got PeerLeft for B");
    assert!(matches!(msg, MeshServerMessage::PeerLeft { .. }));
    // A остался.
    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].peers.len(), 1);
    assert_eq!(snap[0].peers[0].peer_id, peer("A"));
    // _rx_b не используется — B ничего не получает (он удалён).
    drop(rx_b);
}

#[test]
fn different_networks_are_separate() {
    let reg = InMemoryRegistry::new(RelayTable::new(), "r".to_string());
    let _ = join(&reg, net_id(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    let _ = join(&reg, net_id_b(), peer("A"), overlay("10.66.0.1"), "1.1.1.1:1");
    // Одинаковый peer_id и overlay в разных сетях — не коллизия.
    assert_eq!(reg.snapshot().len(), 2);
}
