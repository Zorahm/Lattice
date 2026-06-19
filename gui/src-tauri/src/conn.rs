//! Мост к сетевому backend Lattice. Здесь НЕТ сетевой/крипто-логики — только
//! оркестрация публичного API `lattice-client` в фоновом потоке + трансляция
//! состояния в события Tauri (`status` / `peers` / `diagnostics`), на которые
//! подписан фронт.
//!
//! Почему оркестрация здесь, а не «одна команда backend»: `lattice-client` —
//! CLI-движок с блокирующими циклами (`run_mesh`), без GUI-событий. Мост
//! повторяет тот же жизненный цикл (`establish` → `mesh_session::run` →
//! reconnect), но между шагами снимает состояние пиров и шлёт события. Сам
//! backend не меняется.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use lattice_client::crypto::{Crypto, Key};
use lattice_client::dynamic::Established;
use lattice_client::mesh::{self, MeshError, MeshEstablished, MeshParams};
use lattice_client::mesh_session::{self, MeshSessionCtx, MeshSessionEnd};
use lattice_client::netcfg;
use lattice_client::network_id;
use lattice_client::punch::PunchConfig;
use lattice_client::relay::RelayTransport;
use lattice_client::stun;
use lattice_client::tap::{TapDevice, TapError};
use lattice_client::transport::obfs::JitterPolicy;
use lattice_client::transport::UdpTransport;
use lattice_proto::{OverlayIp, PeerId};

use crate::settings::Settings;

// --- Полезная нагрузка событий (camelCase = TS-типы фронта) ----------------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ErrPayload {
    kind: String,
    detail: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StatusPayload {
    phase: String,
    network: Option<String>,
    overlay_ip: Option<String>,
    error: Option<ErrPayload>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PeerView {
    id: String,
    name: String,
    overlay_ip: String,
    link: String,
    ping_ms: Option<u32>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DiagPayload {
    nat_type: Option<String>,
    external_endpoint: Option<String>,
}

fn emit_status(app: &AppHandle, phase: &str, network: &str, overlay: Option<&str>) {
    let _ = app.emit(
        "status",
        StatusPayload {
            phase: phase.into(),
            network: Some(network.into()),
            overlay_ip: overlay.map(Into::into),
            error: None,
        },
    );
}

fn emit_error(app: &AppHandle, network: &str, kind: &str, detail: String) {
    log::warn!("connect error [{kind}]: {detail}");
    let _ = app.emit(
        "status",
        StatusPayload {
            phase: "error".into(),
            network: Some(network.into()),
            overlay_ip: None,
            error: Some(ErrPayload {
                kind: kind.into(),
                detail: Some(detail),
            }),
        },
    );
}

// --- KDF: пароль+сеть → 32-байтный ключ (только здесь, в Rust) --------------

/// `key = BLAKE3.derive_key("…:<network>", password)`. Детерминированно: те же
/// название+пароль на любой машине дают тот же ключ (и тот же network-id).
/// Название сети служит доменным разделителем (солью) — разные сети с
/// одинаковым паролем не сходятся. Пароль во фронт не возвращается.
fn derive_key(network: &str, password: &str) -> Key {
    let context = format!("lattice-overlay-network-key-v1:{network}");
    let bytes = blake3::derive_key(&context, password.as_bytes());
    Key::new(bytes)
}

fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8), String> {
    let (ip, prefix) = s
        .split_once('/')
        .ok_or_else(|| format!("ожидался IP/PREFIX, получено '{s}'"))?;
    let ip: Ipv4Addr = ip.parse().map_err(|e| format!("плохой IP '{ip}': {e}"))?;
    let prefix: u8 = prefix.parse().map_err(|e| format!("плохой префикс: {e}"))?;
    Ok((ip, prefix))
}

/// Дополнить адрес сервера портом по умолчанию, если он не указан.
fn with_default_port(addr: &str) -> String {
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:51821")
    }
}

fn resolve_stun(specs: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for spec in specs {
        if let Ok(mut addrs) = spec.to_socket_addrs() {
            if let Some(a) = addrs.find(SocketAddr::is_ipv4) {
                out.push(a);
            }
        }
    }
    out
}

fn generate_peer_id() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "peer".to_string());
    let host = host.split('.').next().unwrap_or("peer");
    format!("{host}-{}", std::process::id())
}

// --- Жизненный цикл подключения --------------------------------------------

/// Запустить подключение в фоновом потоке. Возвращает флаг shutdown, которым
/// `disconnect` останавливает поток.
pub fn spawn(
    app: AppHandle,
    settings: Settings,
    network: String,
    password: String,
) -> Arc<AtomicBool> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    std::thread::Builder::new()
        .name("lattice-conn".into())
        .spawn(move || run(&app, &settings, &network, &password, &flag))
        .expect("spawn connection thread");
    shutdown
}

fn run(app: &AppHandle, settings: &Settings, network: &str, password: &str, shutdown: &AtomicBool) {
    emit_status(app, "connecting", network, None);

    // 1. Права администратора (нужны для адаптера/netsh).
    if let Err(e) = netcfg::check_admin() {
        emit_error(
            app,
            network,
            "not_admin",
            e.to_string(),
        );
        return;
    }

    // 2. Параметры из настроек.
    let (ip, prefix) = match parse_cidr(&settings.network.overlay_ip) {
        Ok(v) => v,
        Err(detail) => return emit_error(app, network, "bad_input", detail),
    };
    let key = derive_key(network, password);
    let crypto = Crypto::new(&key);
    let net_id = match network_id::from_key(&key) {
        Ok(id) => id,
        Err(detail) => return emit_error(app, network, "unknown", detail),
    };
    let self_overlay = ip.to_string();

    // 3. Открыть TAP-адаптер.
    let tap = match TapDevice::open() {
        Ok(t) => t,
        Err(TapError::AdapterNotFound(_)) => {
            return emit_error(
                app,
                network,
                "no_tap_driver",
                "tap-windows6 не установлен".into(),
            )
        }
        Err(TapError::NotElevated) => {
            return emit_error(app, network, "not_admin", "нужны права администратора".into())
        }
        Err(e) => return emit_error(app, network, "unknown", e.to_string()),
    };

    // 4. Назначить IP/MTU.
    if let Err(e) = netcfg::configure_interface(&tap.info.name, ip, prefix, settings.network.mtu) {
        return emit_error(app, network, "unknown", format!("настройка адаптера: {e}"));
    }

    // 5. Bind UDP.
    let listen: SocketAddr = format!("0.0.0.0:{}", settings.connection.listen_port)
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
    let transport = match UdpTransport::bind(listen) {
        Ok(t) => t,
        Err(e) => return emit_error(app, network, "unknown", format!("UDP bind: {e}")),
    };

    let stun_servers = resolve_stun(&settings.server.stun);

    // 6. Диагностика: best-effort STUN (тип NAT + внешний endpoint).
    if let Ok(out) = stun::discover(transport.socket(), &stun_servers, Duration::from_secs(3)) {
        let _ = app.emit(
            "diagnostics",
            DiagPayload {
                nat_type: Some(format!("{:?}", out.nat)),
                external_endpoint: Some(out.srflx.to_string()),
            },
        );
    }

    let params = MeshParams {
        rendezvous: with_default_port(&settings.server.coordination),
        network_id: net_id,
        peer_id: PeerId::new(generate_peer_id()),
        overlay_ip: OverlayIp::new(self_overlay.clone()),
        stun_servers,
        connect_timeout: Duration::from_secs(10),
        hello_timeout: Duration::from_secs(15),
        stun_timeout: Duration::from_secs(3),
        punch: PunchConfig::default(),
        heartbeat_interval: Duration::from_secs(settings.connection.keepalive_secs.max(5)),
    };
    let heartbeat = params.heartbeat_interval;

    // 7. Внешний цикл: establish → session → reconnect. Реплика run_mesh,
    //    но с эмиссией событий между шагами.
    let mut roster: BTreeMap<String, PeerView> = BTreeMap::new();
    let mut ever_connected = false;

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match mesh::establish(&transport, &crypto, &params, shutdown) {
            Ok(est) => {
                ever_connected = true;
                update_roster(&mut roster, &est);
                emit_status(app, "connected", network, Some(&self_overlay));
                emit_peers(app, &roster);

                let end = run_session(&tap, &crypto, &transport, &est, heartbeat, shutdown);
                let _ = est
                    .signaling
                    .send(&lattice_proto::mesh::MeshClientMessage::Bye);
                match end {
                    MeshSessionEnd::Shutdown => break,
                    MeshSessionEnd::Kicked | MeshSessionEnd::NetworkClosed => {
                        emit_status(app, "disconnected", network, None);
                        let _ = app.emit("peers", Vec::<PeerView>::new());
                        return;
                    }
                    MeshSessionEnd::LinkDead | MeshSessionEnd::ControlLost => {
                        // Тихий авто-ретрай (без модалок на каждый разрыв).
                        emit_status(app, "reconnecting", network, Some(&self_overlay));
                    }
                }
            }
            Err(MeshError::Aborted) => break,
            Err(e) => {
                let detail = e.to_string();
                if ever_connected {
                    emit_status(app, "reconnecting", network, Some(&self_overlay));
                    if wait_or_stop(shutdown, Duration::from_secs(3)) {
                        break;
                    }
                } else {
                    // Первый коннект не удался — сервер недоступен/отклонил.
                    emit_error(app, network, "server_unreachable", detail);
                    return;
                }
            }
        }
    }

    emit_status(app, "disconnected", network, None);
    let _ = app.emit("peers", Vec::<PeerView>::new());
    drop(tap); // опустить линк адаптера.
    log::info!("connection thread finished");
}

/// Прогнать одну mesh-сессию по решению `Established` (direct или relay).
fn run_session(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &UdpTransport,
    est: &MeshEstablished,
    heartbeat: Duration,
    shutdown: &AtomicBool,
) -> MeshSessionEnd {
    match est.established {
        Established::Direct { .. } => {
            let ctx = MeshSessionCtx {
                tap,
                crypto,
                transport,
                peers: &est.peers,
                signaling: &est.signaling,
                established: &est.established,
                heartbeat_interval: heartbeat,
                heartbeat_jitter: JitterPolicy::fixed(heartbeat),
                shutdown,
            };
            mesh_session::run(&ctx)
        }
        Established::Relay { server, session } => {
            let sock = match transport.socket().try_clone() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("relay socket clone: {e}");
                    return MeshSessionEnd::ControlLost;
                }
            };
            let relay = RelayTransport::new(sock, server, session);
            let _ = relay.send_hello();
            let ctx = MeshSessionCtx {
                tap,
                crypto,
                transport: &relay,
                peers: &est.peers,
                signaling: &est.signaling,
                established: &est.established,
                heartbeat_interval: heartbeat,
                heartbeat_jitter: JitterPolicy::fixed(heartbeat),
                shutdown,
            };
            mesh_session::run(&ctx)
        }
    }
}

/// Обновить реестр пиров из свежего `establish`. Цвет: direct → p2p (зелёный),
/// relay → relay (жёлтый). Пиры, пропавшие из нового списка, помечаются offline
/// (серый) — так список переживает уходы между переустановками.
fn update_roster(roster: &mut BTreeMap<String, PeerView>, est: &MeshEstablished) {
    let link = if matches!(est.established, Established::Direct { .. }) {
        "p2p"
    } else {
        "relay"
    };
    let current = est.peer_ids.read().map(|g| g.clone()).unwrap_or_default();
    let live: std::collections::HashSet<String> =
        current.iter().map(|(pid, _, _)| pid.as_str().to_string()).collect();

    // Сначала всех известных гасим в offline...
    for (id, view) in roster.iter_mut() {
        if !live.contains(id) {
            view.link = "offline".into();
            view.ping_ms = None;
        }
    }
    // ...затем актуальных перезаписываем живым статусом.
    for (pid, _addr, overlay) in &current {
        let id = pid.as_str().to_string();
        roster.insert(
            id.clone(),
            PeerView {
                id,
                name: pid.as_str().to_string(),
                overlay_ip: overlay.as_str().to_string(),
                link: link.into(),
                ping_ms: None,
            },
        );
    }
}

fn emit_peers(app: &AppHandle, roster: &BTreeMap<String, PeerView>) {
    let list: Vec<PeerView> = roster.values().cloned().collect();
    let _ = app.emit("peers", list);
}

/// Подождать `dur`, прерываясь по shutdown. `true` — пора выходить.
fn wait_or_stop(shutdown: &AtomicBool, dur: Duration) -> bool {
    let step = Duration::from_millis(200);
    let mut left = dur;
    while left > Duration::ZERO {
        if shutdown.load(Ordering::Acquire) {
            return true;
        }
        let s = step.min(left);
        std::thread::sleep(s);
        left = left.saturating_sub(s);
    }
    shutdown.load(Ordering::Acquire)
}
