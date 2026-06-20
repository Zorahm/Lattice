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

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use lattice_proto::mesh::{MeshServerMessage, PeerInfo};
use lattice_proto::{OverlayIp, PeerId};

use crate::crypto::Crypto;
use crate::dynamic::Established;
use crate::mesh::MeshSignalRecv;
use crate::mesh::MeshSignaling;
use crate::mesh::lan_target;
use crate::punch::{self, CtrlKind};
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
    /// peer-id → (endpoint, overlay-ip) пиров сети. Обновляется инкрементально из
    /// presence-апдейтов в Direct-режиме (добавить/убрать пира без re-establish).
    pub peer_ids: &'a Arc<RwLock<Vec<(PeerId, SocketAddr, OverlayIp)>>>,
    /// Наш публичный IP (из establish). Пир с тем же IP за тем же NAT → direct
    /// невозможен (hairpin) → переустановка в relay при позднем `PeerJoined`.
    pub self_public_ip: Option<IpAddr>,
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
        peer_ids,
        self_public_ip,
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
            dataplane_keepalive(transport, crypto, established, peers, shutdown, &stop);
        });
        s.spawn(|| {
            control_watch(
                signaling, established, peers, peer_ids, self_public_ip, shutdown, &stop, &reason,
            );
        });
        s.spawn(|| {
            watchdog(established, peers, &last_recv, start, shutdown, &stop, &reason);
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

/// Интервал dataplane-keepalive: 15с — заведомо ниже типичного NAT-таймаута
/// (30-60с), 3 пропуска ещё укладываются в порог. Держит UDP-маппинг открытым.
const DATAPLANE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Dataplane-keepalive mesh-сессии. БЕЗ него NAT-маппинг к relay-серверу (или к
/// direct-пиру) протухает в простое (~30-60с), и форварднутые пакеты упираются в
/// закрытый NAT пира — пир «онлайн» по control-TCP, но UDP-данные до него не
/// доходят. Этого keepalive не было в mesh (в отличие от dynamic-режима), из-за
/// чего relay-путь молча умирал на простаивающей стороне.
///
/// - Relay: пустой relay-hello серверу → сервер освежает запись нашего адреса,
///   наш NAT держит маппинг к серверу открытым (форвард обратно проходит).
/// - Direct: sealed ctrl-keepalive каждому пиру из актуального списка → держит
///   p2p-маппинги; заодно лечит idle-смерть direct-путей (раньше её ловил только
///   watchdog → дорогой re-establish).
fn dataplane_keepalive<T: Transport>(
    transport: &T,
    crypto: &Crypto,
    established: &Established,
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
) {
    // Стартуем «в прошлом», чтобы первый keepalive ушёл сразу — не ждём 15с,
    // пока пир мог простаивать ещё до старта сессии.
    let mut last = Instant::now()
        .checked_sub(DATAPLANE_KEEPALIVE)
        .unwrap_or_else(Instant::now);
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        if last.elapsed() >= DATAPLANE_KEEPALIVE {
            match established {
                Established::Relay { server, .. } => {
                    // Пустой payload → RelayTransport свернёт в relay-hello.
                    // Диагностика: логируем КАЖДУЮ отправку на INFO — если клиент
                    // пишет это, а tcpdump на сервере молчит, UDP режется сетью
                    // между клиентом и relay (а не клиент «не шлёт»).
                    match transport.send(*server, &[]) {
                        Ok(()) => log::info!("mesh: relay keepalive -> {server} (sent)"),
                        Err(e) => log::warn!("mesh: relay keepalive -> {server} failed: {e}"),
                    }
                }
                Established::Direct { .. } => match punch::seal_ctrl(crypto, CtrlKind::Keepalive) {
                    Ok(dg) => {
                        let list = peers.read().map(|g| g.clone()).unwrap_or_default();
                        for p in &list {
                            if let Err(e) = transport.send(*p, &dg) {
                                log::debug!("mesh keepalive: to {p} failed: {e}");
                            }
                        }
                    }
                    Err(e) => log::error!("mesh keepalive: seal failed: {e}"),
                },
            }
            last = Instant::now();
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Обработка Presence-апдейтов из control-канала.
///
/// ## Почему presence НЕ триггерит re-establish
///
/// Раньше любой `PeerJoined`/`PeerLeft`/`PeerUpdated` в Direct-режиме рвал сессию
/// (`LinkDead`) ради re-punch. Это самоиндуцирующийся storm: re-establish =
/// reconnect к координатору, а reconnect порождает `PeerJoined` у ВСЕХ остальных
/// пиров → они тоже рвут сессию → у всех снова Join → вечный цикл. Сессии живут
/// <1с, ARP/данные не успевают пройти.
///
/// Вместо этого presence обновляет список пиров инкрементально, не трогая
/// датаплейн-циклы: `peers` (broadcast-таргеты в `tap_to_net_mesh`) и `peer_ids`
/// (для отчётов). Для cone-NAT (Direct выбирается только если все стартовые
/// punch'и удались → NAT не симметричный) встречный поток данных сам открывает
/// путь к добавленному пиру — отдельный re-punch не нужен. Re-establish остаётся
/// только на реальные сбои: watchdog (тишина direct-пути) и `ControlLost`.
fn control_watch(
    signaling: &MeshSignaling,
    established: &Established,
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
    peer_ids: &Arc<RwLock<Vec<(PeerId, SocketAddr, OverlayIp)>>>,
    self_public_ip: Option<IpAddr>,
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
                // Direct: добавляем endpoint в broadcast-список (без re-establish).
                // Пир за тем же NAT → его LAN-адрес (прямой путь по локалке), иначе
                // публичный srflx. Re-establish из-за same-NAT больше НЕ делаем —
                // именно он раньше вызывал флап (storm переподключений).
                // Relay: relay сам пересылает всем — ничего не делаем.
                if matches!(established, Established::Direct { .. }) {
                    match direct_target(&info, self_public_ip) {
                        Some(addr) => upsert_direct_peer(peers, peer_ids, &info, addr),
                        None => log::warn!(
                            "mesh: peer {} has bad srflx '{}'; skipped",
                            info.peer_id.as_str(),
                            info.srflx
                        ),
                    }
                } else {
                    log::info!("mesh: peer {} joined (relay routes; no-op)", info.peer_id.as_str());
                }
            }
            MeshSignalRecv::Message(MeshServerMessage::PeerLeft { peer_id }) => {
                if matches!(established, Established::Direct { .. }) {
                    remove_direct_peer(peers, peer_ids, &peer_id);
                } else {
                    log::info!("mesh: peer {} left (relay routes; no-op)", peer_id.as_str());
                }
            }
            MeshSignalRecv::Message(MeshServerMessage::PeerUpdated(info)) => {
                if matches!(established, Established::Direct { .. }) {
                    match direct_target(&info, self_public_ip) {
                        Some(addr) => upsert_direct_peer(peers, peer_ids, &info, addr),
                        None => log::warn!(
                            "mesh: peer {} has bad srflx '{}'; skipped",
                            info.peer_id.as_str(),
                            info.srflx
                        ),
                    }
                } else {
                    log::info!("mesh: peer {} updated (relay routes; no-op)", info.peer_id.as_str());
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

/// Адрес для прямого коннекта к пиру: LAN-адрес, если он за тем же NAT, иначе
/// публичный srflx. `None` — оба адреса не разобрались (битый пир). Сравнение
/// NAT — по публичному IP (см. `mesh::lan_target`).
fn direct_target(info: &PeerInfo, self_ip: Option<IpAddr>) -> Option<SocketAddr> {
    lan_target(info, self_ip).or_else(|| info.srflx.parse().ok())
}

/// Добавить (или обновить endpoint) direct-пира в broadcast-список и реестр id.
/// Идемпотентно: повторный `PeerJoined` того же пира лишь обновит адрес. `addr` —
/// уже выбранная цель (LAN или публичный srflx, см. `direct_target`); для cone-NAT
/// и пиров за одним NAT датаплейн сам пробьёт путь встречным трафиком.
fn upsert_direct_peer(
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
    peer_ids: &Arc<RwLock<Vec<(PeerId, SocketAddr, OverlayIp)>>>,
    info: &PeerInfo,
    addr: SocketAddr,
) {
    if let Ok(mut ids) = peer_ids.write() {
        if let Some(entry) = ids.iter_mut().find(|(id, _, _)| *id == info.peer_id) {
            entry.1 = addr;
            entry.2 = info.overlay_ip.clone();
        } else {
            ids.push((info.peer_id.clone(), addr, info.overlay_ip.clone()));
        }
    }
    if let Ok(mut p) = peers.write() {
        if !p.contains(&addr) {
            p.push(addr);
        }
    }
    log::info!(
        "mesh: direct peer {} at {} (overlay {}) added/updated; no re-establish",
        info.peer_id.as_str(),
        addr,
        info.overlay_ip.as_str()
    );
}

/// Убрать ушедшего direct-пира из broadcast-списка и реестра id по peer-id.
fn remove_direct_peer(
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
    peer_ids: &Arc<RwLock<Vec<(PeerId, SocketAddr, OverlayIp)>>>,
    peer_id: &PeerId,
) {
    let removed = peer_ids.write().ok().and_then(|mut ids| {
        ids.iter()
            .position(|(id, _, _)| id == peer_id)
            .map(|i| ids.remove(i).1)
    });
    if let Some(addr) = removed {
        if let Ok(mut p) = peers.write() {
            p.retain(|a| *a != addr);
        }
        log::info!("mesh: direct peer {} ({}) left; removed", peer_id.as_str(), addr);
    } else {
        log::info!("mesh: peer {} left (not in direct set)", peer_id.as_str());
    }
}

/// Watchdog direct-пути: тишина > 60с (~3×keepalive) → `LinkDead` (re-establish).
/// В relay-режиме не активен — relay-путь рвётся через control (`ControlLost`).
///
/// Молчание считаем ТОЛЬКО когда в сети есть хотя бы один пир: одинокий пир
/// данных не получает по определению, и без этой проверки watchdog рвал бы его
/// сессию каждые 60с — самоиндуцированный флап (network create/emptied в логах
/// сервера). Отсчёт тишины ведём от момента, когда пиры появились (или от
/// последнего принятого пакета, если он свежее) — чтобы только что вошедшему
/// пиру давался полный таймаут, а не мгновенный разрыв из-за старого `last_recv`.
fn watchdog(
    established: &Established,
    peers: &Arc<RwLock<Vec<SocketAddr>>>,
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
    let mut active_since: Option<u64> = None; // elapsed_ms, когда появились пиры.
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        let now = elapsed_ms(start);
        let has_peers = peers.read().is_ok_and(|p| !p.is_empty());
        if has_peers {
            let base = active_since.get_or_insert(now);
            let baseline = (*base).max(last_recv.load(Ordering::Acquire));
            let silent = now.saturating_sub(baseline);
            if Duration::from_millis(silent) > dead_after {
                log::warn!("mesh: direct link silent for {silent}ms; re-establishing");
                signal_end(reason, stop, MeshSessionEnd::LinkDead);
                return;
            }
        } else {
            active_since = None; // одни — отсчёт тишины не ведём.
        }
        thread::sleep(Duration::from_millis(500));
    }
}
