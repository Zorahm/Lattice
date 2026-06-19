//! UDP relay-ретранслятор датаплейна (fallback при провале punch).
//!
//! Сервер тупой: пакет на relay-сокете → по `session` найти сеть → переслать
//! `payload` всем участникам кроме отправителя. **Ключа сервер не имеет;
//! `payload` — ciphertext датаплейна `[nonce || AEAD(frame)]`.** E2E из SPEC не
//! ослабляется: relay видит только зашифрованные байты и адреса, не содержимое.
//!
//! Фаза 3: одна relay-сессия на сеть (не на пару) — broadcast-модель TAP-overlay
//! рассылает фреймы всем пирам, это совпадает с тем, что relay пересылает каждому
//! кроме отправителя, и не требует per-pair session. Фаза 2 (2 пира на комнату)
//! продолжает работать на том же `RelayTable` — просто список `addrs` короче.
//!
//! Адреса участников relay узнаёт из source UDP-пакетов (не из control-канала):
//! внешний адрес для датаплейна может отличаться от control-TCP (другой сокет →
//! другой NAT-маппинг). Поэтому клиент при переходе в relay шлёт пустой
//! «hello»-пакет — сервер записывает его адрес ещё до потока данных.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use lattice_proto::relay as wire;

/// Состояние одной relay-сессии: внешние датаплейн-адреса её участников
/// (без上限а на Фазе 3 — сеть может быть >2 пиров). Заполняется по мере прихода
/// пакетов; дедуп по `contains` — повторный hello от того же адреса не раздувает
/// список.
#[derive(Default)]
struct RelaySession {
    addrs: Vec<SocketAddr>,
}

/// Таблица активных relay-сессий. Делится между control-потоками (открывают/
/// закрывают сессии при join/leave сети) и relay-потоком (читает/дописывает
/// адреса). Клонируется дёшево — внутри `Arc`.
#[derive(Clone, Default)]
pub struct RelayTable {
    inner: Arc<Mutex<HashMap<u64, RelaySession>>>,
}

impl RelayTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Открыть сессию при создании сети / матче пары. До этого пакеты с её
    /// `session` дропаются — чтобы relay не пересылал трафик «сетей», которых
    /// сервер не сводил.
    pub fn open(&self, session: u64) {
        if let Ok(mut t) = self.inner.lock() {
            t.insert(session, RelaySession::default());
        }
    }

    /// Закрыть сессию при teardown сети/комнаты — дальнейшие пакеты дропаются.
    pub fn close(&self, session: u64) {
        if let Ok(mut t) = self.inner.lock() {
            t.remove(&session);
        }
    }

    /// Зарегистрировать адрес отправителя и вернуть адреса, КУДА пересылать
    /// (все участники сессии, кроме самого отправителя). `None` — сессия
    /// неизвестна (дроп). `Some(vec![])` — известная сессия, но некому
    /// пересылать (только отправитель; повторный hello не раздувает список).
    fn route(&self, session: u64, from: SocketAddr) -> Option<Vec<SocketAddr>> {
        let mut t = self.inner.lock().ok()?;
        let sess = t.get_mut(&session)?;
        if !sess.addrs.contains(&from) {
            // Без верхнего лимита (Фаза 3 — сеть до N пиров), но реальный
            // источник сессии — только `join` через control-канал; самозванец,
            // угадавший u64, лишь добавит себя в список и будет получать чужой
            // ciphertext (расшифровать не сможет — ключа нет), E2E не рвётся.
            sess.addrs.push(from);
        }
        Some(sess.addrs.iter().copied().filter(|a| *a != from).collect())
    }
}

/// Цикл обслуживания relay-сокета. Блокирующий; запускается в своём потоке.
/// Никогда не возвращается штатно — крутится, пока процесс жив. Ошибки одного
/// `recv`/`send` логируются и не роняют цикл (контракт: relay-канал устойчив).
pub fn serve(socket: &UdpSocket, table: &RelayTable) {
    // 64 KiB — потолок UDP-датаграммы. payload не больше датаплейн-датаграммы
    // (≈ MTU + overhead), но читаем с запасом, чтобы негабарит не обрезался.
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("relay: recv_from failed: {e}");
                continue;
            }
        };
        let Some((session, payload)) = wire::decode(&buf[..n]) else {
            log::trace!("relay: dropped {n}B non-relay datagram from {from}");
            continue;
        };
        let Some(targets) = table.route(session, from) else {
            log::trace!("relay: unknown session {session} from {from}, dropped");
            continue;
        };
        // Пустой payload = hello/keepalive: адрес уже записан в route(), пересылать
        // нечего. Иначе ретранслируем ciphertext каждому другому участнику.
        if payload.is_empty() {
            continue;
        }
        let framed = wire::encode(session, payload);
        for target in targets {
            if let Err(e) = socket.send_to(&framed, target) {
                log::warn!("relay: forward to {target} (session {session}) failed: {e}");
            }
        }
    }
}
