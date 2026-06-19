//! Датаплейн-сессия: два цикла tap↔transport + keepalive/watchdog.
//!
//! Циклы обобщены по `trait Transport`, поэтому direct (`UdpTransport`) и relay
//! (`RelayTransport`) гоняются одним кодом — меняется только реализация
//! транспорта (AGENTS.md «Сменяемый транспорт»). Статический режим Фазы 1
//! (`run_static`) использует те же циклы без сигналинга/keepalive — Фаза 1 не
//! переписана, лишь переиспользована.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::crypto::Crypto;
use crate::peers::Discovery;
use crate::punch::{self, CtrlKind};
use crate::signaling::{SignalRecv, SignalingClient};
use crate::tap::{TapDevice, TapError, FRAME_BUF_LEN};
use crate::transport::obfs::JitterPolicy;
use crate::transport::{Transport, TransportError, RECV_BUF_LEN};

use lattice_proto::ServerMessage;

/// Чем завершилась сессия — определяет, переустанавливать ли соединение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// Глобальный shutdown (Ctrl+C) — выходим совсем.
    Shutdown,
    /// Пир сообщил об уходе (control `PeerGone`) — выходим.
    PeerGone,
    /// Оборвался control-канал к серверу — выходим с внятной причиной.
    ControlLost,
    /// Прямой путь замолчал дольше таймаута (NAT-биндинг протух, keepalive не
    /// помог) — переустанавливаем соединение через rendezvous, не теряем связь молча.
    LinkDead,
}

/// Что слать как keepalive, чтобы NAT-биндинг (или relay-маппинг) не протух.
#[derive(Debug, Clone, Copy)]
pub enum Keepalive {
    /// Direct: периодический зашифрованный ctrl-пакет пиру.
    DirectPing(SocketAddr),
    /// Relay: пустой relay-«hello» серверу (держит его знание нашего адреса).
    RelayHello(SocketAddr),
}

/// План keepalive: что слать и как часто. `jitter` (Фаза 4) рандомизирует
/// КАДЕНЦИЮ отправки служебных пакетов — ломает машинно-регулярный ритм
/// keepalive (пассивный признак). По умолчанию `JitterPolicy::fixed(interval)`
/// → детерминированно как раньше (Фазы 1-3 не меняются). Watchdog «мёртвого»
/// пути по-прежнему считает по базовому `interval` (jitter не должен влиять на
/// порог переустановки).
#[derive(Debug, Clone, Copy)]
pub struct KeepalivePlan {
    pub kind: Keepalive,
    pub interval: Duration,
    pub jitter: JitterPolicy,
}

/// Phase 1: статический mesh из `--peer`. Без сигналинга и keepalive — ровно
/// поведение Фазы 1. Возвращается по глобальному shutdown.
pub fn run_static<T: Transport + Sync, D: Discovery + Sync>(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &T,
    peers: &D,
    shutdown: &AtomicBool,
) {
    let never = AtomicBool::new(false);
    let last_recv = AtomicU64::new(0);
    let start = Instant::now();
    thread::scope(|s| {
        s.spawn(|| tap_to_net(tap, crypto, transport, peers, shutdown, &never));
        s.spawn(|| net_to_tap(tap, crypto, transport, shutdown, &never, &last_recv, start));
    });
}

/// Phase 2: динамическая сессия (direct или relay) с keepalive, watchdog и
/// наблюдением за control-каналом. Возвращает причину завершения.
pub fn run_dynamic<T: Transport + Sync, D: Discovery + Sync>(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &T,
    peers: &D,
    keepalive: KeepalivePlan,
    signaling: &SignalingClient,
    shutdown: &AtomicBool,
) -> SessionEnd {
    let stop = AtomicBool::new(false);
    let reason: Mutex<Option<SessionEnd>> = Mutex::new(None);
    let start = Instant::now();
    // last_recv инициализируем «сейчас»: сессия стартует с живым путём (punch
    // только что прошёл / relay согласован), watchdog не должен сработать сразу.
    let last_recv = AtomicU64::new(elapsed_ms(start));

    thread::scope(|s| {
        s.spawn(|| tap_to_net(tap, crypto, transport, peers, shutdown, &stop));
        s.spawn(|| net_to_tap(tap, crypto, transport, shutdown, &stop, &last_recv, start));
        s.spawn(|| {
            keepalive_loop(transport, crypto, keepalive, &last_recv, start, shutdown, &stop, &reason);
        });
        s.spawn(|| control_watch(signaling, shutdown, &stop, &reason));
    });

    if shutdown.load(Ordering::Acquire) {
        return SessionEnd::Shutdown;
    }
    reason
        .lock()
        .ok()
        .and_then(|g| *g)
        .unwrap_or(SessionEnd::Shutdown)
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn signal_end(reason: &Mutex<Option<SessionEnd>>, stop: &AtomicBool, end: SessionEnd) {
    if let Ok(mut g) = reason.lock() {
        if g.is_none() {
            *g = Some(end); // первый победитель фиксирует причину.
        }
    }
    stop.store(true, Ordering::Release);
}

#[inline]
fn stopped(shutdown: &AtomicBool, stop: &AtomicBool) -> bool {
    shutdown.load(Ordering::Acquire) || stop.load(Ordering::Acquire)
}

/// tap → net: читаем Ethernet-фрейм, шифруем, рассылаем пирам. Горячий путь —
/// без unwrap; ошибки логируются, цикл продолжается.
pub(crate) fn tap_to_net<T: Transport, D: Discovery>(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &T,
    peers: &D,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut frame = vec![0u8; FRAME_BUF_LEN];
    let peer_list = match peers.peers() {
        Ok(p) => p.to_vec(),
        Err(e) => {
            log::error!("tap->net: no peers: {e}");
            return;
        }
    };
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        let n = match tap.read_frame(&mut frame) {
            Ok(n) => n,
            Err(TapError::WouldBlock) => continue,
            Err(e) => {
                log::warn!("tap->net: read error: {e}");
                continue;
            }
        };
        let datagram = match crypto.seal(&frame[..n]) {
            Ok(d) => d,
            Err(e) => {
                log::error!("tap->net: seal failed: {e}");
                continue;
            }
        };
        for peer in &peer_list {
            if let Err(e) = transport.send(*peer, &datagram) {
                log::warn!("tap->net: send to {peer} failed: {e}");
            }
        }
    }
}

/// net → tap: принимаем датаграмму, расшифровываем, отсеиваем control-пакеты
/// (punch/keepalive), остальное пишем в TAP. Обновляет `last_recv` для watchdog.
pub(crate) fn net_to_tap<T: Transport>(
    tap: &TapDevice,
    crypto: &Crypto,
    transport: &T,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
    last_recv: &AtomicU64,
    start: Instant,
) {
    let mut buf = vec![0u8; RECV_BUF_LEN];
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        let (n, from) = match transport.recv(&mut buf) {
            Ok(pair) => pair,
            Err(TransportError::WouldBlock) => continue,
            Err(e) => {
                log::warn!("net->tap: recv error: {e}");
                continue;
            }
        };
        let Some(frame) = crypto.open(&buf[..n]) else {
            log::trace!("net->tap: dropped {n}B datagram from {from}");
            continue;
        };
        // Любой валидный (расшифрованный) пакет от пира — признак живого пути.
        last_recv.store(elapsed_ms(start), Ordering::Release);
        // Control-пакеты (punch ping/pong, keepalive) в TAP не пишем.
        if punch::control_kind(&frame).is_some() {
            continue;
        }
        if let Err(e) = tap.write_frame(&frame) {
            log::warn!("net->tap: TAP write error: {e}");
        }
    }
}

/// Периодический keepalive + watchdog «мёртвого» direct-пути.
#[allow(clippy::too_many_arguments)]
fn keepalive_loop<T: Transport>(
    transport: &T,
    crypto: &Crypto,
    keepalive: KeepalivePlan,
    last_recv: &AtomicU64,
    start: Instant,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
    reason: &Mutex<Option<SessionEnd>>,
) {
    let interval = keepalive.interval;
    // Путь считаем мёртвым после 3 пропущенных keepalive (с запасом против
    // одиночных потерь) — тогда инициируем переустановку через rendezvous.
    // По БАЗОВОМУ интервалу, не jitter'ом: порог должен быть стабильным.
    let dead_after = interval * 3;
    // Стартуем так, чтобы первый keepalive ушёл сразу.
    let mut last_sent = Instant::now()
        .checked_sub(interval)
        .unwrap_or_else(Instant::now);
    // Текущая (возможно jitter'нутая) цель интервала до следующего keepalive.
    let mut next_gap = keepalive.jitter.next_interval();
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        if last_sent.elapsed() >= next_gap {
            send_keepalive(transport, crypto, keepalive.kind);
            last_sent = Instant::now();
            next_gap = keepalive.jitter.next_interval(); // новый jitter на след. раз.
        }
        // Watchdog только для direct: relay-путь рвётся через control (ControlLost).
        if let Keepalive::DirectPing(_) = keepalive.kind {
            let silent = elapsed_ms(start).saturating_sub(last_recv.load(Ordering::Acquire));
            if Duration::from_millis(silent) > dead_after {
                log::warn!("direct link silent for {silent}ms; re-establishing via rendezvous");
                signal_end(reason, stop, SessionEnd::LinkDead);
                return;
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn send_keepalive<T: Transport>(transport: &T, crypto: &Crypto, keepalive: Keepalive) {
    match keepalive {
        Keepalive::DirectPing(peer) => match punch::seal_ctrl(crypto, CtrlKind::Keepalive) {
            Ok(dg) => {
                if let Err(e) = transport.send(peer, &dg) {
                    log::debug!("keepalive: send to {peer} failed: {e}");
                }
            }
            Err(e) => log::error!("keepalive: seal failed: {e}"),
        },
        // Пустой payload → RelayTransport свернёт его в relay-hello.
        Keepalive::RelayHello(server) => {
            if let Err(e) = transport.send(server, &[]) {
                log::debug!("keepalive: relay hello to {server} failed: {e}");
            }
        }
    }
}

/// Наблюдение за control-каналом: `PeerGone`/обрыв завершают сессию с причиной.
fn control_watch(
    signaling: &SignalingClient,
    shutdown: &AtomicBool,
    stop: &AtomicBool,
    reason: &Mutex<Option<SessionEnd>>,
) {
    loop {
        if stopped(shutdown, stop) {
            return;
        }
        match signaling.recv(Duration::from_millis(500)) {
            SignalRecv::Message(ServerMessage::PeerGone) => {
                log::info!("peer left the session");
                signal_end(reason, stop, SessionEnd::PeerGone);
                return;
            }
            SignalRecv::Closed => {
                log::warn!("control channel lost");
                signal_end(reason, stop, SessionEnd::ControlLost);
                return;
            }
            SignalRecv::Message(other) => log::debug!("control: ignoring {other:?} mid-session"),
            SignalRecv::Timeout => {}
        }
    }
}
