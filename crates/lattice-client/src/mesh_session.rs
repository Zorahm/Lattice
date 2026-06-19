//! Датаплейн-сессия mesh-режима: tap↔transport циклы + heartbeat к серверу +
//! Presence-обработка (`PeerJoined`/`PeerLeft`/`PeerUpdated`) + watchdog.
//!
//! Переиспользует `session::net_to_tap` (общий для всех режимов — демультиплекси-
//! рование control-пакетов, обновление `last_recv` для watchdog). `tap_to_net`
//! свой: mesh-список пиров живёт в `Arc<RwLock<Vec<SocketAddr>>>` и обновляется
//! из control-потока, а `trait Discovery` Фазы 1/2 возвращает `&[SocketAddr]` из
//! `&self` — не совместимо с `RwLock`-guard'ом. Дублирование ~25 строк дешевле,
//! чем менять контракт трейта и ломать static/dynamic.
//!
//! ## Presence → reconnect
//!
//! В direct-режиме приход нового пира (`PeerJoined`) или уход (`PeerLeft`)
//! требуют re-punch/rebuild списка direct endpoint'ов — это полный re-establish
//! (внешний цикл в `run_mesh`). В relay-режиме relay-сервер сам маршрутизирует
//! всем, новый пир сразу получит данные — re-establish не нужен, только
//! обновляем `peer_ids` для корректности PunchOk-отчётов. Watchdog по тишине
//! direct-пути → `LinkDead` → re-establish (как в Фазе 2).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use lattice_proto::mesh::MeshServerMessage;

use crate::crypto::Crypto;
use crate::dynamic::Established;
use crate::mesh::MeshSignalRecv;
use crate::mesh::MeshSignaling;
use crate::session::net_to_tap;
use crate::transport::obfs::JitterPolicy;
use crate::tap::{TapDevice, TapError, FRAME_BUF_LEN};
use crate::transport::Transport;

/// Чем завершилась mesh-сессия — определяет, переустанавливать ли соединение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshSessionEnd {
    Shutdown,
    /// Control-канал оборвался — re-establish (внешний цикл).
    ControlLost,
    /// Direct-путь замолчал дольше watchdog-таймаута — re-establish.
    LinkDead,
    /// Пира кикнули (админка) — выходим, не reconnect.
    Kicked,
    /// Сеть закрыта админкой — выходим.
    NetworkClosed,
}

/// Контекст mesh-сессии: ссылки на разделяемые ресурсы + параметры. Вынесен в
/// структуру, чтобы `run` не нёс 8 позиционных аргументов
/// (`clippy::too_many_arguments`). Все поля — заимствования; `run` живёт в
/// `thread::scope`, так что `'static` не нужен.
pub struct MeshSessionCtx<'a, T: Transport> {
    pub tap: &'a TapDevice,
    pub crypto: &'a Crypto,
    pub transport: &'a T,
    pub peers: &'a Arc<RwLock<Vec<SocketAddr>>>,
    pub signaling: &'a MeshSignaling,
    pub established: &'a Established,
    pub heartbeat_interval: Duration,
    /// jitter каденции heartbeat (Фаза 4). `fixed(heartbeat_interval)` по
    /// умолчанию → как раньше.
    pub heartbeat_jitter: JitterPolicy,
    pub shutdown: &'a AtomicBool,
}

/// Прогнать mesh-сессию. Generic по транспорту: caller (`run_mesh` в `run.rs`)
/// строит `UdpTransport` (all-direct) или `RelayTransport` (all-relay) по
/// `Established` и передаёт сюда через `MeshSessionCtx`. Возвращает причину
/// завершения.
#[must_use]
pub fn run<T: Transport + Sync>(ctx: &MeshSessionCtx<'_, T>) -> MeshSessionEnd {
    let MeshSessionCtx {
        tap,
        crypto,
        transport,
        peers,
        signaling,
        established,
        heartbeat_interval,
        heartbeat_jitter,
        shutdown,
    } = *ctx;
    let stop = AtomicBool::new(false);
    let reason: Mutex<Option<MeshSessionEnd>> = Mutex::new(None);
    let start = Instant::now();
    let last_recv = AtomicU64::new(elapsed_ms(start));

    // Без `move`: stop/reason/last_recv — owned-локали, их надо разделить по
    // ссылке между всеми пятью scoped-потоками (signal_end/net_to_tap/watchdog
    // берут `&`). `thread::scope` гарантирует join до конца `run`, поэтому
    // заимствование окружения безопасно, а `move` здесь сломал бы шеринг.
    thread::scope(|s| {
        s.spawn(|| tap_to_net_mesh(tap, crypto, transport, peers, shutdown, &stop));
        s.spawn(|| {
            net_to_tap(tap, crypto, transport, shutdown, &stop, &last_recv, start);
        });
        s.spawn(|| {
            heartbeat_loop(signaling, heartbeat_interval, heartbeat_jitter, shutdown, &stop);
        });
        s.spawn(|| {
            control_watch(signaling, established, shutdown, &stop, &reason);
        });
        s.spawn(|| {
            watchdog(established, &last_recv, start, shutdown, &stop, &reason);
        });
    });

    if shutdown.load(Ordering::Acquire) {
        return MeshSessionEnd::Shutdown;
    }
    reason
        .lock()
        .ok()
        .and_then(|g| *g)
        .unwrap_or(MeshSessionEnd::Shutdown)
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn signal_end(reason: &Mutex<Option<MeshSessionEnd>>, stop: &AtomicBool, end: MeshSessionEnd) {
    if let Ok(mut g) = reason.lock() {
        if g.is_none() {
            *g = Some(end);
        }
    }
    stop.store(true, Ordering::Release);
}

#[inline]
fn stopped(shutdown: &AtomicBool, stop: &AtomicBool) -> bool {
    shutdown.load(Ordering::Acquire) || stop.load(Ordering::Acquire)
}

/// tap → net для mesh: читаем фрейм, шифруем, рассылаем всем пирам из
/// `RwLock<Vec<SocketAddr>>` (обновляется из control-потока). Горячий путь —
/// без unwrap, ошибки логируются.
fn tap_to_net_mesh<T: Transport>(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &T,
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut frame = vec![0u8; FRAME_BUF_LEN];
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        let n = match tap.read_frame(&mut frame) {
            Ok(n) => n,
            Err(TapError::WouldBlock) => continue,
            Err(e) => {
                log::warn!("mesh tap->net: read error: {e}");
                continue;
            }
        };
        let datagram = match crypto.seal(&frame[..n]) {
            Ok(d) => d,
            Err(e) => {
                log::error!("mesh tap->net: seal failed: {e}");
                continue;
            }
        };
        // Снимок списка пиров под read-lock; рассылаем каждому. В relay-режиме
        // список = [relay_server], relay пересылает всем кроме отправителя.
        let peer_list = match peers.read() {
            Ok(g) => g.clone(),
            Err(_) => continue, // отравлен lock — пропускаем фрейм, не падаем.
        };
        for peer in &peer_list {
            if let Err(e) = transport.send(*peer, &datagram) {
                log::warn!("mesh tap->net: send to {peer} failed: {e}");
            }
        }
    }
}

/// Heartbeat к coordination-серверу: каждые `interval` шлём `Heartbeat`, чтобы
/// presence не помечало нас offline (порог — 3 пропуска ~45с). `MeshSignaling`
/// держит write-half под `Mutex`, так что heartbeat-поток и punch-отчёты не
/// конфликтуют (contention нулевой — heartbeat редкий).
fn heartbeat_loop(
    signaling: &MeshSignaling,
    interval: Duration,
    jitter: JitterPolicy,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut last = Instant::now()
        .checked_sub(interval)
        .unwrap_or_else(Instant::now);
    let mut next_gap = jitter.next_interval();
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        if last.elapsed() >= next_gap {
            if let Err(e) = signaling.send(&lattice_proto::mesh::MeshClientMessage::Heartbeat) {
                log::warn!("mesh: heartbeat send failed: {e}");
            }
            last = Instant::now();
            next_gap = jitter.next_interval(); // новый jitter на след. heartbeat.
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Обработка Presence-апдейтов из control-канала.
fn control_watch(
    signaling: &MeshSignaling,
    established: &Established,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
    reason: &Mutex<Option<MeshSessionEnd>>,
) {
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        match signaling.recv(Duration::from_millis(500)) {
            MeshSignalRecv::Message(MeshServerMessage::PeerJoined(info)) => {
                log::info!("mesh: peer {} joined", info.peer_id.as_str());
                // Direct-режим: новый пир требует re-punch → re-establish.
                // Relay-режим: relay сам пересылает всем, ничего не делаем.
                if matches!(established, Established::Direct { .. }) {
                    signal_end(reason, stop, MeshSessionEnd::LinkDead);
                    return;
                }
            }
            MeshSignalRecv::Message(MeshServerMessage::PeerLeft { peer_id }) => {
                log::info!("mesh: peer {} left", peer_id.as_str());
                if matches!(established, Established::Direct { .. }) {
                    signal_end(reason, stop, MeshSessionEnd::LinkDead);
                    return;
                }
            }
            MeshSignalRecv::Message(MeshServerMessage::PeerUpdated(info)) => {
                log::info!("mesh: peer {} updated endpoint", info.peer_id.as_str());
                if matches!(established, Established::Direct { .. }) {
                    signal_end(reason, stop, MeshSessionEnd::LinkDead);
                    return;
                }
            }
            MeshSignalRecv::Message(MeshServerMessage::Kicked { reason: r }) => {
                log::warn!("mesh: kicked by admin: {r}");
                signal_end(reason, stop, MeshSessionEnd::Kicked);
                return;
            }
            MeshSignalRecv::Message(MeshServerMessage::NetworkClosed { reason: r }) => {
                log::warn!("mesh: network closed by admin: {r}");
                signal_end(reason, stop, MeshSessionEnd::NetworkClosed);
                return;
            }
            MeshSignalRecv::Message(MeshServerMessage::Error { message }) => {
                log::warn!("mesh: server error: {message}");
            }
            MeshSignalRecv::Message(other) => {
                log::debug!("mesh: ignoring {other:?}");
            }
            MeshSignalRecv::Timeout => {}
            MeshSignalRecv::Closed => {
                log::warn!("mesh: control channel lost");
                signal_end(reason, stop, MeshSessionEnd::ControlLost);
                return;
            }
        }
    }
}

/// Watchdog direct-пути: тишина > 60с (~3×keepalive) → `LinkDead` (re-establish).
/// В relay-режиме не активен — relay-путь рвётся через control (`ControlLost`).
fn watchdog(
    established: &Established,
    last_recv: &AtomicU64,
    start: Instant,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
    reason: &Mutex<Option<MeshSessionEnd>>,
) {
    if !matches!(established, Established::Direct { .. }) {
        return; // relay — не watchdog'им.
    }
    let dead_after = Duration::from_secs(60);
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        let silent = elapsed_ms(start).saturating_sub(last_recv.load(Ordering::Acquire));
        if Duration::from_millis(silent) > dead_after {
            log::warn!("mesh: direct link silent for {silent}ms; re-establishing");
            signal_end(reason, stop, MeshSessionEnd::LinkDead);
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}
