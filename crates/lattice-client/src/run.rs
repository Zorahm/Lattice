//! Бинд транспорта и запуск выбранного режима. Динамический режим и mesh —
//! внешний цикл переустановки: при `LinkDead` (протух NAT-биндинг / Presence-
//! апдейт) переустанавливаемся через coordination-сервер, а не теряем связь
//! молча.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lattice_client::crypto::Crypto;
use lattice_client::dynamic::{establish as establish_dynamic, Established, EstablishError};
use lattice_client::mesh::{self, establish as establish_mesh, MeshError};
use lattice_client::mesh_session::{self, MeshSessionEnd};
use lattice_client::peers::{DynamicPeers, StaticPeers};
use lattice_client::relay::RelayTransport;
use lattice_client::session::{self, Keepalive, KeepalivePlan, SessionEnd};
use lattice_client::tap::TapDevice;
use lattice_client::transport::obfs::{JitterPolicy, ObfsTransport, PaddingPolicy};
use lattice_client::transport::selector::TransportPreference;
use lattice_client::transport::{Transport, UdpTransport};
use lattice_proto::ClientMessage;

use crate::cli::{DynamicRun, MeshRun, StaticParams, TransportConfig};

/// Залогировать план транспорта и провалидировать QUIC-конфиг на старте.
/// Fail-fast: опечатка в `--sni` / битый ALPN должны падать сразу, а не при
/// эскалации в бою (edge case «handshake не проходит → лог, не падать молча»).
///
/// # Errors
///
/// Текстовая ошибка, если при QUIC-режиме `--sni` невалиден.
pub fn announce_transport(cfg: &TransportConfig) -> Result<(), String> {
    log::info!(
        "transport: preference={:?}, padding={}, jitter={}",
        cfg.preference,
        cfg.padding.is_some(),
        cfg.jitter
    );
    // При любом режиме, где возможен QUIC, проверяем, что TLS/SNI собираются.
    if !matches!(cfg.preference, TransportPreference::Udp) {
        lattice_client::transport::quic_tls::server_config(&cfg.sni)
            .map_err(|e| format!("invalid QUIC --sni '{}': {e}", cfg.sni))?;
        log::info!(
            "QUIC transport available (ALPN h3, SNI '{}'). Честная планка: маскировка \
             под H3 против сигнатуры/пассивной эвристики, не против активного пробинга.",
            cfg.sni
        );
    }
    Ok(())
}

/// jitter-политика каденции служебных пакетов из конфига. Включён → ±25% вокруг
/// `base`, не ниже `base/2` (слишком частый keepalive сам сигнатура и грузит
/// сеть); выключен → `fixed(base)` (детерминированно, как Фазы 1-3).
fn service_jitter(cfg: &TransportConfig, base: Duration) -> JitterPolicy {
    if cfg.jitter {
        JitterPolicy {
            base,
            spread: base / 4,
            floor: base / 2,
        }
    } else {
        JitterPolicy::fixed(base)
    }
}

/// Обернуть ПРЯМОЙ датаплейн-транспорт в obfs-padding, если включён. Только для
/// direct-путей: relay-путь держит wire-конвенцию Фаз 1-3 (пустой payload =
/// relay-hello), а obfs-префикс её ломает; padding на relay и не нужен — там
/// трафик и так клиент↔сервер. `Box<dyn>`, чтобы не дублировать вызов сессии.
fn direct_dataplane<'a, T: Transport + Send + Sync + 'a>(
    t: T,
    padding: Option<PaddingPolicy>,
) -> Box<dyn Transport + Send + Sync + 'a> {
    match padding {
        Some(p) => Box::new(ObfsTransport::new(t, p)),
        None => Box::new(t),
    }
}

/// Phase 1: статический mesh. Поведение Фазы 1 без изменений.
pub fn run_static(
    tap: &TapDevice,
    crypto: &Crypto,
    params: &StaticParams,
    cfg: &TransportConfig,
    shutdown: &AtomicBool,
) -> ExitCode {
    if params.peers.is_empty() {
        eprintln!("error: no peers configured (pass at least one --peer)");
        return ExitCode::from(2);
    }
    let transport = match UdpTransport::bind(params.listen) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: UDP bind {} failed: {e}", params.listen);
            return ExitCode::from(5);
        }
    };
    log::info!("UDP bound on {}; static mesh of {} peer(s)", params.listen, params.peers.len());
    let peers = StaticPeers::new(params.peers.clone());
    // Static — прямой путь (no NAT/punch), QUIC тут не согласуешь (нет
    // signaling для ролей), поэтому транспорт UDP (+ optional obfs-padding).
    let dataplane = direct_dataplane(&transport, cfg.padding);
    session::run_static(tap, crypto, &dataplane, &peers, shutdown);
    ExitCode::SUCCESS
}

/// Phase 2: NAT traversal через rendezvous.
pub fn run_dynamic(
    tap: &TapDevice,
    crypto: &Crypto,
    run: &DynamicRun,
    cfg: &TransportConfig,
    shutdown: &AtomicBool,
) -> ExitCode {
    let transport = match UdpTransport::bind(run.listen) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: UDP bind {} failed: {e}", run.listen);
            return ExitCode::from(5);
        }
    };
    log::info!(
        "UDP bound on {}; rendezvous {} room '{}'",
        transport.local_addr().map_or(run.listen, |a| a),
        run.params.rendezvous,
        run.params.room.as_str()
    );

    // Внешний цикл: одна итерация = одна сессия. LinkDead → переустановка.
    loop {
        if shutdown.load(Ordering::Acquire) {
            return ExitCode::SUCCESS;
        }
        match establish_dynamic(&transport, crypto, &run.params, shutdown) {
            Ok((established, signaling)) => {
                let end = run_one_session(
                    tap, crypto, &transport, run, cfg, established, &signaling, shutdown,
                );
                let mut signaling = signaling;
                let _ = signaling.send(&ClientMessage::Bye); // best-effort teardown.
                match end {
                    SessionEnd::LinkDead => {
                        log::info!("re-establishing session after dead link");
                        // следующая итерация цикла.
                    }
                    SessionEnd::Shutdown => return ExitCode::SUCCESS,
                    SessionEnd::PeerGone => {
                        log::info!("peer gone; exiting");
                        return ExitCode::SUCCESS;
                    }
                    SessionEnd::ControlLost => {
                        log::warn!("control channel lost; exiting");
                        return ExitCode::SUCCESS;
                    }
                }
            }
            Err(EstablishError::Aborted) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: connection setup failed: {e}");
                return ExitCode::from(6);
            }
        }
    }
}

/// Построить транспорт по итогу установления и прогнать одну сессию.
#[allow(clippy::too_many_arguments)]
fn run_one_session(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &UdpTransport,
    run: &DynamicRun,
    cfg: &TransportConfig,
    established: Established,
    signaling: &lattice_client::signaling::SignalingClient,
    shutdown: &AtomicBool,
) -> SessionEnd {
    let jitter = service_jitter(cfg, run.keepalive);
    match established {
        Established::Direct { peer } => {
            let peers = DynamicPeers::new(peer);
            // Direct — можно маскировать padding'ом (оба пира с одним флагом).
            let dataplane = direct_dataplane(transport, cfg.padding);
            session::run_dynamic(
                tap,
                crypto,
                &dataplane,
                &peers,
                KeepalivePlan {
                    kind: Keepalive::DirectPing(peer),
                    interval: run.keepalive,
                    jitter,
                },
                signaling,
                shutdown,
            )
        }
        Established::Relay { server, session: sess } => {
            // try_clone датаплейн-сокета: тот же внешний маппинг, что узнал
            // сервер по hello. Отдельный сокет нам не нужен.
            let sock = match transport.socket().try_clone() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("cannot clone socket for relay: {e}");
                    return SessionEnd::ControlLost;
                }
            };
            let relay = RelayTransport::new(sock, server, sess);
            let _ = relay.send_hello(); // сразу сообщаем серверу свой адрес.
            let peers = DynamicPeers::new(server);
            // Relay БЕЗ obfs: пустой payload = relay-hello (см. direct_dataplane);
            // obfs-префикс сломал бы конвенцию, а сервер мы не трогаем.
            session::run_dynamic(
                tap,
                crypto,
                &relay,
                &peers,
                KeepalivePlan {
                    kind: Keepalive::RelayHello(server),
                    interval: run.keepalive,
                    jitter,
                },
                signaling,
                shutdown,
            )
        }
    }
}

/// Phase 3: mesh coordination. Внешний цикл переустановки: при `LinkDead`
/// (Presence-апдейт в direct-режиме / протухший путь) или `ControlLost` —
/// переустанавливаемся через coordination-сервер (re-`Hello`), mesh
/// восстанавливается из живых. `Kicked`/`NetworkClosed` — выход без reconnect.
pub fn run_mesh(
    tap: &TapDevice,
    crypto: &Crypto,
    run: &MeshRun,
    cfg: &TransportConfig,
    shutdown: &AtomicBool,
) -> ExitCode {
    let transport = match UdpTransport::bind(run.listen) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: UDP bind {} failed: {e}", run.listen);
            return ExitCode::from(5);
        }
    };
    log::info!(
        "UDP bound on {}; mesh coordination {} network {} peer {}",
        transport.local_addr().map_or(run.listen, |a| a),
        run.params.rendezvous,
        run.params.network_id.as_str(),
        run.params.peer_id.as_str()
    );

    loop {
        if shutdown.load(Ordering::Acquire) {
            return ExitCode::SUCCESS;
        }
        match establish_mesh(&transport, crypto, &run.params, shutdown) {
            Ok(established) => {
                let end =
                    run_one_mesh_session(tap, crypto, &transport, &established, run, cfg, shutdown);
                let _ = established
                    .signaling
                    .send(&lattice_proto::mesh::MeshClientMessage::Bye);
                match end {
                    MeshSessionEnd::LinkDead => {
                        log::info!("mesh: re-establishing after link-dead/presence change");
                    }
                    MeshSessionEnd::ControlLost => {
                        log::warn!("mesh: control lost; re-establishing");
                    }
                    MeshSessionEnd::Shutdown => return ExitCode::SUCCESS,
                    MeshSessionEnd::Kicked => {
                        log::info!("mesh: kicked, exiting");
                        return ExitCode::SUCCESS;
                    }
                    MeshSessionEnd::NetworkClosed => {
                        log::info!("mesh: network closed, exiting");
                        return ExitCode::SUCCESS;
                    }
                }
            }
            Err(MeshError::Aborted) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: mesh setup failed: {e}");
                return ExitCode::from(6);
            }
        }
    }
}

/// Построить транспорт по `Established` и прогнать одну mesh-сессию.
fn run_one_mesh_session(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &UdpTransport,
    established: &mesh::MeshEstablished,
    run: &MeshRun,
    cfg: &TransportConfig,
    shutdown: &AtomicBool,
) -> MeshSessionEnd {
    let heartbeat_jitter = service_jitter(cfg, run.heartbeat);
    match established.established {
        Established::Direct { .. } => {
            // Mesh-direct — прямые пути ко всем пирам; obfs-padding применим
            // (все пиры сети с одним флагом). QUIC per-peer — отдельный seam.
            let dataplane = direct_dataplane(transport, cfg.padding);
            let ctx = mesh_session::MeshSessionCtx {
                tap,
                crypto,
                transport: &dataplane,
                peers: &established.peers,
                peer_ids: &established.peer_ids,
                self_public_ip: established.self_public_ip,
                signaling: &established.signaling,
                established: &established.established,
                heartbeat_interval: run.heartbeat,
                heartbeat_jitter,
                shutdown,
            };
            mesh_session::run(&ctx)
        }
        Established::Relay { server, session: sess } => {
            // Relay-транспорт: try_clone датаплейн-сокета (тот же NAT-маппинг),
            // send_hello сразу, чтобы сервер узнал наш адрес до потока данных.
            let sock = match transport.socket().try_clone() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("mesh: cannot clone socket for relay: {e}");
                    return MeshSessionEnd::ControlLost;
                }
            };
            let relay = RelayTransport::new(sock, server, sess);
            let _ = relay.send_hello();
            // Relay без obfs (см. run_one_session): пустой payload = relay-hello.
            let ctx = mesh_session::MeshSessionCtx {
                tap,
                crypto,
                transport: &relay,
                peers: &established.peers,
                peer_ids: &established.peer_ids,
                self_public_ip: established.self_public_ip,
                signaling: &established.signaling,
                established: &established.established,
                heartbeat_interval: run.heartbeat,
                heartbeat_jitter,
                shutdown,
            };
            mesh_session::run(&ctx)
        }
    }
}
